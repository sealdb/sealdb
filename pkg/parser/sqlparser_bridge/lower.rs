use super::dialect::dialect;
use crate::ast::Statement;
use crate::ast::ddl::{
    AlterTableSpec, AlterTableStmt, ColumnDef, CreateDatabaseStmt, CreateIndexStmt,
    CreateTableStmt, DropDatabaseStmt, DropIndexStmt, DropTableStmt, IndexDef, PartitionDef,
    PartitionOption, PartitionType, TableName,
};
use crate::ast::dml::{
    Assignment, ByItem, CommonTableExpression, DeleteStmt, InsertStmt, Join, JoinType, SelectField,
    SelectLock, SelectStmt, SetOprStmt, SetOprType, UpdateStmt, WithClause,
};
use crate::ast::expressions::{
    BinaryOpExpr, ExistsExpr, ExprNode, FuncCallExpr, FuncCastExpr, InListExpr, InSubqueryExpr,
    ScalarSubqueryExpr, UnaryOpExpr, ValueExpr, WindowFuncExpr, WindowSpec,
};
use crate::ast::misc::{BeginStmt, CommitStmt, RollbackStmt, ShowStmt, ShowStmtKind, UseStmt};
use crate::opcode::Op;
use sealdb_errno::{ERR_PARSE_ERROR, ERR_UNSUPPORTED, Error, Result};
use sealdb_types::Datum;
use sqlparser::ast::{
    AlterTableOperation, AssignmentTarget, BinaryOperator, Cte, CteAsMaterialized, DataType,
    Delete, DescribeAlias, Expr, FromTable, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, GroupByExpr, JoinConstraint, JoinOperator, LimitClause, LockClause,
    LockType, NonBlock, ObjectName, ObjectType, OrderByExpr, OrderByKind, OrderBySort, Query,
    SchemaName, SelectItem, SetExpr, SetOperator, SetQuantifier, Statement as SqlStatement,
    TableConstraint, TableFactor, TableObject, TableWithJoins, UnaryOperator, Update, Use, Value,
    Values, WindowType, With,
};
use sqlparser::parser::Parser;

pub fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    let (rewritten, pessimistic_flags) = rewrite_begin_pessimistic(sql);
    let (rewritten, partitions) = extract_create_table_partitions(&rewritten)?;
    let stmts = Parser::parse_sql(&dialect(), &rewritten).map_err(|e| {
        Error::new(
            ERR_PARSE_ERROR,
            format!("You have an error in your SQL syntax: {e}"),
        )
    })?;
    let mut flags = pessimistic_flags.into_iter();
    let mut partitions = partitions.into_iter();
    stmts
        .iter()
        .map(|stmt| {
            let mut lowered = lower_statement(stmt)?;
            if let Statement::Begin(begin) = &mut lowered
                && flags.next() == Some(true)
            {
                begin.pessimistic = true;
            }
            if let Statement::CreateTable(create) = &mut lowered {
                create.partition = partitions.next().flatten();
            }
            Ok(lowered)
        })
        .collect()
}

fn lower_statement(stmt: &SqlStatement) -> Result<Statement> {
    match stmt {
        SqlStatement::Query(query) => lower_query_stmt(query),
        SqlStatement::CreateDatabase {
            db_name,
            if_not_exists,
            ..
        } => Ok(Statement::CreateDatabase(CreateDatabaseStmt {
            if_not_exists: *if_not_exists,
            name: object_name_last(db_name)?,
        })),
        SqlStatement::CreateSchema {
            schema_name,
            if_not_exists,
            ..
        } => Ok(Statement::CreateDatabase(CreateDatabaseStmt {
            if_not_exists: *if_not_exists,
            name: schema_name_last(schema_name)?,
        })),
        SqlStatement::CreateTable(create) => {
            // F02/F18 gaps: TEMPORARY, LIKE, AS SELECT stay deferred.
            if create.temporary || create.query.is_some() || create.like.is_some() {
                return Err(Error::unsupported(
                    "TEMPORARY / LIKE / AS SELECT CREATE TABLE",
                ));
            }
            let cols = create
                .columns
                .iter()
                .map(|col| {
                    Ok(ColumnDef {
                        name: col.name.value.clone(),
                        type_name: col.data_type.to_string(),
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if cols.is_empty() {
                return Err(Error::unsupported("CREATE TABLE without columns"));
            }
            let indices = lower_table_indices(&create.constraints)?;
            Ok(Statement::CreateTable(CreateTableStmt {
                if_not_exists: create.if_not_exists,
                table: object_table_name(&create.name)?,
                cols,
                indices,
                partition: None,
            }))
        }
        SqlStatement::CreateIndex(create) => lower_create_index(create),
        SqlStatement::Drop {
            object_type: ObjectType::Index,
            if_exists,
            names,
            table,
            ..
        } => {
            let table = table
                .as_ref()
                .ok_or_else(|| Error::unsupported("DROP INDEX without ON"))?;
            let name = names
                .first()
                .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "missing index name"))?;
            if names.len() != 1 {
                return Err(Error::unsupported("DROP INDEX multiple names"));
            }
            Ok(Statement::DropIndex(DropIndexStmt {
                if_exists: *if_exists,
                table: object_table_name(table)?,
                name: object_name_last(name)?,
            }))
        }
        SqlStatement::AlterTable(alter) => lower_alter_table(alter),
        SqlStatement::Drop {
            object_type,
            if_exists,
            names,
            temporary,
            ..
        } => {
            if *temporary {
                return Err(Error::unsupported("DROP TEMPORARY"));
            }
            match object_type {
                ObjectType::Database | ObjectType::Schema => {
                    let name = names
                        .first()
                        .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "missing database name"))?;
                    Ok(Statement::DropDatabase(DropDatabaseStmt {
                        if_exists: *if_exists,
                        name: object_name_last(name)?,
                    }))
                }
                ObjectType::Table => Ok(Statement::DropTable(DropTableStmt {
                    if_exists: *if_exists,
                    tables: names
                        .iter()
                        .map(object_table_name)
                        .collect::<Result<Vec<_>>>()?,
                })),
                other => Err(Error::unsupported(format!("DROP {other}"))),
            }
        }
        SqlStatement::ShowDatabases { .. } | SqlStatement::ShowSchemas { .. } => {
            Ok(Statement::Show(ShowStmt {
                kind: ShowStmtKind::Databases,
                db_name: None,
                table: None,
            }))
        }
        SqlStatement::ShowTables { show_options, .. } => Ok(Statement::Show(ShowStmt {
            kind: ShowStmtKind::Tables,
            db_name: show_options
                .show_in
                .as_ref()
                .and_then(|inn| inn.parent_name.as_ref())
                .map(object_name_last)
                .transpose()?,
            table: None,
        })),
        SqlStatement::ShowColumns { show_options, .. } => {
            let table = show_options
                .show_in
                .as_ref()
                .and_then(|inn| inn.parent_name.as_ref())
                .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "missing table name"))?;
            Ok(Statement::Show(ShowStmt {
                kind: ShowStmtKind::Columns,
                db_name: None,
                table: Some(object_table_name(table)?),
            }))
        }
        SqlStatement::ExplainTable {
            describe_alias,
            table_name,
            ..
        } => {
            if !matches!(
                describe_alias,
                DescribeAlias::Desc | DescribeAlias::Describe
            ) {
                return Err(Error::unsupported(format!("EXPLAIN {describe_alias}")));
            }
            Ok(Statement::Show(ShowStmt {
                kind: ShowStmtKind::Columns,
                db_name: None,
                table: Some(object_table_name(table_name)?),
            }))
        }
        SqlStatement::Insert(insert) => {
            let table = match &insert.table {
                TableObject::TableName(name) => object_table_name(name)?,
                other => return Err(Error::unsupported(format!("INSERT target {other}"))),
            };
            let columns = insert
                .columns
                .iter()
                .map(object_name_last)
                .collect::<Result<Vec<_>>>()?;
            let rows = match insert.source.as_deref().map(|q| q.body.as_ref()) {
                Some(SetExpr::Values(values)) => lower_values(values)?,
                Some(other) => {
                    return Err(Error::unsupported(format!("INSERT source {other}")));
                }
                None => return Err(Error::unsupported("INSERT without VALUES")),
            };
            Ok(Statement::Insert(InsertStmt {
                table,
                columns,
                rows,
            }))
        }
        SqlStatement::Update(update) => lower_update(update),
        SqlStatement::Delete(delete) => lower_delete(delete),
        SqlStatement::Use(use_stmt) => {
            let name = match use_stmt {
                Use::Catalog(name)
                | Use::Schema(name)
                | Use::Database(name)
                | Use::Object(name) => object_name_last(name)?,
                other => return Err(Error::unsupported(format!("USE {other}"))),
            };
            Ok(Statement::Use(UseStmt { db_name: name }))
        }
        SqlStatement::StartTransaction {
            modes,
            statements,
            exception,
            ..
        } => {
            // F12/F13 gap: isolation modes, read-only, causal consistency,
            // BEGIN ... END blocks. BEGIN PESSIMISTIC is rewritten before parse.
            if !modes.is_empty() {
                return Err(Error::unsupported("START TRANSACTION modes"));
            }
            if !statements.is_empty() || exception.is_some() {
                return Err(Error::unsupported("BEGIN block"));
            }
            Ok(Statement::Begin(BeginStmt { pessimistic: false }))
        }
        SqlStatement::Commit {
            chain, modifier, ..
        } => {
            if *chain || modifier.is_some() {
                return Err(Error::unsupported("COMMIT AND CHAIN"));
            }
            Ok(Statement::Commit(CommitStmt))
        }
        SqlStatement::Rollback { chain, savepoint } => {
            // F12 gap / completeness wave: SAVEPOINT and AND CHAIN.
            if *chain || savepoint.is_some() {
                return Err(Error::unsupported("ROLLBACK TO SAVEPOINT"));
            }
            Ok(Statement::Rollback(RollbackStmt))
        }
        SqlStatement::Savepoint { .. } | SqlStatement::ReleaseSavepoint { .. } => {
            Err(Error::unsupported("SAVEPOINT"))
        }
        other => Err(Error::new(
            ERR_UNSUPPORTED,
            format!("statement {other} is not available in this phase"),
        )),
    }
}

fn rewrite_begin_pessimistic(sql: &str) -> (String, Vec<bool>) {
    let mut out = String::with_capacity(sql.len());
    let mut flags = Vec::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "START") {
            let after_start = skip_ws(bytes, i + 5);
            if keyword_at(bytes, after_start, "TRANSACTION") {
                let after_txn = skip_ws(bytes, after_start + "TRANSACTION".len());
                let pessimistic = keyword_at(bytes, after_txn, "PESSIMISTIC");
                flags.push(pessimistic);
                out.push_str(&sql[i..after_start + "TRANSACTION".len()]);
                i = if pessimistic {
                    after_txn + "PESSIMISTIC".len()
                } else {
                    after_start + "TRANSACTION".len()
                };
                continue;
            }
        }
        if keyword_at(bytes, i, "BEGIN") {
            let after = skip_ws(bytes, i + 5);
            let pessimistic = keyword_at(bytes, after, "PESSIMISTIC");
            flags.push(pessimistic);
            out.push_str("BEGIN");
            i = if pessimistic {
                after + "PESSIMISTIC".len()
            } else {
                i + 5
            };
            continue;
        }
        out.push(sql[i..].chars().next().unwrap());
        i += sql[i..].chars().next().unwrap().len_utf8();
    }
    (out, flags)
}

fn keyword_at(bytes: &[u8], i: usize, keyword: &str) -> bool {
    let end = i + keyword.len();
    if end > bytes.len() {
        return false;
    }
    if i > 0 && is_ident_byte(bytes[i - 1]) {
        return false;
    }
    if end < bytes.len() && is_ident_byte(bytes[end]) {
        return false;
    }
    bytes[i..end].eq_ignore_ascii_case(keyword.as_bytes())
}

fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// sqlparser-rs MySQL dialect does not accept TiDB/MySQL `PARTITION BY`.
/// Strip the clause and re-attach it after lowering, same pattern as
/// `BEGIN PESSIMISTIC`.
fn extract_create_table_partitions(sql: &str) -> Result<(String, Vec<Option<PartitionOption>>)> {
    let bytes = sql.as_bytes();
    let mut out = String::new();
    let mut opts = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if keyword_at(bytes, i, "CREATE") {
            let after_create = skip_ws(bytes, i + "CREATE".len());
            if keyword_at(bytes, after_create, "TABLE") {
                let after_table = skip_ws(bytes, after_create + "TABLE".len());
                let after_name = skip_create_table_name(bytes, after_table);
                let after_name = skip_ws(bytes, after_name);
                if after_name < bytes.len()
                    && bytes[after_name] == b'('
                    && let Some(close) = matching_paren(bytes, after_name)
                {
                    let after_cols = skip_ws(bytes, close + 1);
                    if keyword_at(bytes, after_cols, "PARTITION") {
                        let after_part = skip_ws(bytes, after_cols + "PARTITION".len());
                        if keyword_at(bytes, after_part, "BY") {
                            let start = after_part + "BY".len();
                            let (opt, end) = parse_partition_by(bytes, skip_ws(bytes, start))?;
                            out.push_str(&sql[i..after_cols]);
                            opts.push(Some(opt));
                            i = end;
                            continue;
                        }
                    }
                    out.push_str(&sql[i..close + 1]);
                    opts.push(None);
                    i = close + 1;
                    continue;
                }
            }
        }
        out.push(sql[i..].chars().next().unwrap());
        i += sql[i..].chars().next().unwrap().len_utf8();
    }
    Ok((out, opts))
}

fn skip_create_table_name(bytes: &[u8], mut i: usize) -> usize {
    i = skip_ws(bytes, i);
    if keyword_at(bytes, i, "IF") {
        let after_if = skip_ws(bytes, i + 2);
        if keyword_at(bytes, after_if, "NOT") {
            let after_not = skip_ws(bytes, after_if + 3);
            if keyword_at(bytes, after_not, "EXISTS") {
                i = skip_ws(bytes, after_not + "EXISTS".len());
            }
        }
    }
    i = skip_ident_or_quoted(bytes, i);
    i = skip_ws(bytes, i);
    if i < bytes.len() && bytes[i] == b'.' {
        i = skip_ident_or_quoted(bytes, skip_ws(bytes, i + 1));
    }
    i
}

fn skip_ident_or_quoted(bytes: &[u8], mut i: usize) -> usize {
    if i >= bytes.len() {
        return i;
    }
    match bytes[i] {
        b'`' | b'"' | b'\'' => {
            let q = bytes[i];
            i += 1;
            while i < bytes.len() && bytes[i] != q {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            i
        }
        _ => {
            while i < bytes.len() && is_ident_byte(bytes[i]) {
                i += 1;
            }
            i
        }
    }
}

fn matching_paren(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn parse_partition_by(bytes: &[u8], mut i: usize) -> Result<(PartitionOption, usize)> {
    // F19 gap: LIST / KEY / RANGE COLUMNS / subpartition / expr keys.
    if keyword_at(bytes, i, "LIST")
        || keyword_at(bytes, i, "KEY")
        || (keyword_at(bytes, i, "RANGE")
            && keyword_at(bytes, skip_ws(bytes, i + "RANGE".len()), "COLUMNS"))
    {
        return Err(Error::unsupported("LIST/KEY/RANGE COLUMNS partition"));
    }
    let tp = if keyword_at(bytes, i, "RANGE") {
        i = skip_ws(bytes, i + "RANGE".len());
        PartitionType::Range
    } else if keyword_at(bytes, i, "HASH") {
        i = skip_ws(bytes, i + "HASH".len());
        PartitionType::Hash
    } else {
        return Err(Error::unsupported("partition type"));
    };
    if i >= bytes.len() || bytes[i] != b'(' {
        return Err(Error::unsupported("PARTITION BY expression"));
    }
    let close = matching_paren(bytes, i).ok_or_else(|| Error::unsupported("PARTITION BY"))?;
    let column = std::str::from_utf8(&bytes[i + 1..close])
        .map_err(|_| Error::new(ERR_PARSE_ERROR, "invalid partition column"))?
        .trim()
        .trim_matches('`')
        .to_string();
    if column.is_empty() || column.contains('(') {
        return Err(Error::unsupported("partition expression"));
    }
    i = skip_ws(bytes, close + 1);
    match tp {
        PartitionType::Hash => {
            if !keyword_at(bytes, i, "PARTITIONS") {
                return Err(Error::unsupported("HASH partition list"));
            }
            i = skip_ws(bytes, i + "PARTITIONS".len());
            let (num, end) = parse_u64_at(bytes, i)?;
            if num == 0 {
                return Err(Error::unsupported("HASH PARTITIONS 0"));
            }
            Ok((
                PartitionOption {
                    tp,
                    column,
                    defs: Vec::new(),
                    num,
                },
                end,
            ))
        }
        PartitionType::Range => {
            if i >= bytes.len() || bytes[i] != b'(' {
                return Err(Error::unsupported("RANGE without partitions"));
            }
            let end = matching_paren(bytes, i)
                .ok_or_else(|| Error::unsupported("RANGE partition list"))?;
            let body = std::str::from_utf8(&bytes[i + 1..end])
                .map_err(|_| Error::new(ERR_PARSE_ERROR, "invalid RANGE partitions"))?;
            let defs = parse_range_defs(body)?;
            Ok((
                PartitionOption {
                    tp,
                    column,
                    num: defs.len() as u64,
                    defs,
                },
                end + 1,
            ))
        }
    }
}

fn parse_range_defs(body: &str) -> Result<Vec<PartitionDef>> {
    let mut defs = Vec::new();
    for raw in split_defs(body) {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        let bytes = item.as_bytes();
        let mut i = skip_ws(bytes, 0);
        if !keyword_at(bytes, i, "PARTITION") {
            return Err(Error::unsupported("RANGE partition item"));
        }
        i = skip_ws(bytes, i + "PARTITION".len());
        let name_end = skip_ident_or_quoted(bytes, i);
        let name = item[i..name_end].trim().trim_matches('`').to_string();
        i = skip_ws(bytes, name_end);
        if !keyword_at(bytes, i, "VALUES") {
            return Err(Error::unsupported("VALUES LESS THAN"));
        }
        i = skip_ws(bytes, i + "VALUES".len());
        if !keyword_at(bytes, i, "LESS") {
            return Err(Error::unsupported("VALUES LESS THAN"));
        }
        i = skip_ws(bytes, i + "LESS".len());
        if !keyword_at(bytes, i, "THAN") {
            return Err(Error::unsupported("VALUES LESS THAN"));
        }
        i = skip_ws(bytes, i + "THAN".len());
        let less_than = if keyword_at(bytes, i, "MAXVALUE") {
            None
        } else {
            if i >= bytes.len() || bytes[i] != b'(' {
                return Err(Error::unsupported("VALUES LESS THAN"));
            }
            let close = matching_paren(bytes, i).ok_or_else(|| Error::unsupported("LESS THAN"))?;
            let bound = item[i + 1..close].trim();
            if bound.eq_ignore_ascii_case("MAXVALUE") {
                None
            } else {
                Some(
                    bound
                        .parse::<i64>()
                        .map_err(|_| Error::unsupported("non-integer LESS THAN"))?,
                )
            }
        };
        defs.push(PartitionDef { name, less_than });
    }
    if defs.is_empty() {
        return Err(Error::unsupported("RANGE without partitions"));
    }
    Ok(defs)
}

fn split_defs(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0;
    for ch in body.chars() {
        match ch {
            '(' => {
                depth += 1;
                cur.push(ch);
            }
            ')' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn parse_u64_at(bytes: &[u8], i: usize) -> Result<(u64, usize)> {
    let mut end = i;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == i {
        return Err(Error::new(ERR_PARSE_ERROR, "missing PARTITIONS count"));
    }
    let n = std::str::from_utf8(&bytes[i..end])
        .map_err(|_| Error::new(ERR_PARSE_ERROR, "invalid PARTITIONS count"))?
        .parse::<u64>()
        .map_err(|_| Error::new(ERR_PARSE_ERROR, "invalid PARTITIONS count"))?;
    Ok((n, end))
}

fn lower_select_lock(locks: &[LockClause]) -> Result<SelectLock> {
    if locks.is_empty() {
        return Ok(SelectLock::None);
    }
    if locks.len() != 1 {
        return Err(Error::unsupported("multiple SELECT lock clauses"));
    }
    let lock = &locks[0];
    if lock.of.is_some() {
        return Err(Error::unsupported("FOR UPDATE OF"));
    }
    match lock.nonblock {
        Some(NonBlock::Nowait) => return Err(Error::unsupported("FOR UPDATE NOWAIT")),
        Some(NonBlock::SkipLocked) => return Err(Error::unsupported("FOR UPDATE SKIP LOCKED")),
        None => {}
    }
    match lock.lock_type {
        LockType::Update => Ok(SelectLock::ForUpdate),
        LockType::Share => Err(Error::unsupported("FOR SHARE")),
    }
}

fn lower_query_stmt(query: &Query) -> Result<Statement> {
    let with = lower_with(query.with.as_ref())?;
    match query.body.as_ref() {
        SetExpr::Select(_) => {
            let mut select = lower_query(query)?;
            select.with = with;
            Ok(Statement::Select(select))
        }
        SetExpr::SetOperation { .. } => {
            let mut set = lower_set_opr(query)?;
            set.with = with;
            Ok(Statement::SetOpr(set))
        }
        SetExpr::Query(inner) => {
            if with.is_some() && inner.with.is_some() {
                return Err(Error::unsupported("nested WITH"));
            }
            let mut stmt = lower_query_stmt(inner)?;
            if let Some(with) = with {
                match &mut stmt {
                    Statement::Select(select) if select.with.is_none() => select.with = Some(with),
                    Statement::SetOpr(set) if set.with.is_none() => set.with = Some(with),
                    Statement::Select(_) | Statement::SetOpr(_) => {
                        return Err(Error::unsupported("nested WITH"));
                    }
                    _ => return Err(Error::unsupported("WITH on non-SELECT")),
                }
            }
            Ok(stmt)
        }
        other => Err(Error::new(
            ERR_UNSUPPORTED,
            format!("query body {:?} is not available in this phase", other),
        )),
    }
}

fn lower_set_opr(query: &Query) -> Result<SetOprStmt> {
    let (first, rest) = flatten_set_expr(query.body.as_ref())?;
    let (limit, offset) = lower_limit(query.limit_clause.as_ref())?;
    Ok(SetOprStmt {
        first,
        rest,
        order_by: lower_order_by(query.order_by.as_ref())?,
        limit,
        offset,
        with: None,
    })
}

fn lower_with(with: Option<&With>) -> Result<Option<WithClause>> {
    let Some(with) = with else {
        return Ok(None);
    };
    // F17 gap / completeness wave: recursive CTE, MATERIALIZED, UNION in CTE.
    if with.recursive {
        return Err(Error::unsupported("WITH RECURSIVE"));
    }
    let mut names = Vec::new();
    let mut ctes = Vec::with_capacity(with.cte_tables.len());
    for cte in &with.cte_tables {
        let lowered = lower_cte(cte)?;
        if names
            .iter()
            .any(|name: &String| name.eq_ignore_ascii_case(&lowered.name))
        {
            return Err(Error::unsupported("duplicate CTE name"));
        }
        names.push(lowered.name.clone());
        ctes.push(lowered);
    }
    Ok(Some(WithClause {
        is_recursive: false,
        ctes,
    }))
}

fn lower_cte(cte: &Cte) -> Result<CommonTableExpression> {
    if cte.materialized.is_some() {
        return Err(Error::unsupported(match cte.materialized {
            Some(CteAsMaterialized::Materialized) => "MATERIALIZED CTE",
            Some(CteAsMaterialized::NotMaterialized) => "NOT MATERIALIZED CTE",
            None => "MATERIALIZED CTE",
        }));
    }
    if cte.from.is_some() {
        return Err(Error::unsupported("CTE FROM clause"));
    }
    if cte.query.with.is_some() {
        return Err(Error::unsupported("nested WITH"));
    }
    let query = match cte.query.body.as_ref() {
        SetExpr::Select(_) => lower_query(&cte.query)?,
        SetExpr::SetOperation { .. } => return Err(Error::unsupported("UNION in CTE")),
        other => return Err(Error::unsupported(format!("CTE body {other}"))),
    };
    Ok(CommonTableExpression {
        name: cte.alias.name.value.clone(),
        query: Box::new(query),
        col_name_list: cte
            .alias
            .columns
            .iter()
            .map(|col| col.name.value.clone())
            .collect(),
    })
}

fn flatten_set_expr(expr: &SetExpr) -> Result<(SelectStmt, Vec<(SetOprType, SelectStmt)>)> {
    match expr {
        SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => {
            let tp = map_set_opr(*op, *set_quantifier)?;
            match left.as_ref() {
                SetExpr::SetOperation { .. } => {
                    let (first, mut rest) = flatten_set_expr(left)?;
                    rest.push((tp, lower_set_operand(right)?));
                    Ok((first, rest))
                }
                _ => Ok((
                    lower_set_operand(left)?,
                    vec![(tp, lower_set_operand(right)?)],
                )),
            }
        }
        other => Err(Error::unsupported(format!("set operand {other}"))),
    }
}

fn lower_set_operand(expr: &SetExpr) -> Result<SelectStmt> {
    match expr {
        SetExpr::Select(select) => lower_select(
            select,
            None,
            None,
            None,
            SelectLock::None,
            select.to_string(),
        ),
        SetExpr::Query(query) => lower_query(query),
        // F15 gap: parenthesized nested UNION as a single operand.
        SetExpr::SetOperation { .. } => Err(Error::unsupported("nested parenthesized UNION")),
        other => Err(Error::unsupported(format!("UNION operand {other}"))),
    }
}

fn map_set_opr(op: SetOperator, quantifier: SetQuantifier) -> Result<SetOprType> {
    match (op, quantifier) {
        (SetOperator::Union, SetQuantifier::All) => Ok(SetOprType::UnionAll),
        (SetOperator::Union, SetQuantifier::None | SetQuantifier::Distinct) => {
            Ok(SetOprType::Union)
        }
        (SetOperator::Except, _) => Err(Error::unsupported("EXCEPT")),
        (SetOperator::Intersect, _) => Err(Error::unsupported("INTERSECT")),
        (SetOperator::Minus, _) => Err(Error::unsupported("MINUS")),
        (_, other) => Err(Error::unsupported(format!("UNION {other}"))),
    }
}

fn lower_query(query: &Query) -> Result<SelectStmt> {
    match query.body.as_ref() {
        SetExpr::Select(select) => {
            let (limit, offset) = lower_limit(query.limit_clause.as_ref())?;
            lower_select(
                select,
                query.order_by.as_ref(),
                limit,
                offset,
                lower_select_lock(&query.locks)?,
                query.to_string(),
            )
        }
        // F15 gap / completeness wave: UNION inside scalar/EXISTS/IN subquery.
        other => Err(Error::new(
            ERR_UNSUPPORTED,
            format!("query body {:?} is not available in this phase", other),
        )),
    }
}

fn lower_select(
    select: &sqlparser::ast::Select,
    order_by: Option<&sqlparser::ast::OrderBy>,
    limit: Option<u64>,
    offset: Option<u64>,
    lock: SelectLock,
    text: String,
) -> Result<SelectStmt> {
    let fields = select
        .projection
        .iter()
        .map(lower_select_item)
        .collect::<Result<Vec<_>>>()?;
    let (from, join) = lower_from(&select.from)?;
    let selection = select.selection.as_ref().map(lower_expr).transpose()?;
    // F08 gap / completeness wave: HAVING.
    if select.having.is_some() {
        return Err(Error::unsupported("HAVING"));
    }
    let group_by = lower_group_by(&select.group_by)?;
    let order_by = lower_order_by(order_by)?;
    Ok(SelectStmt {
        fields,
        from,
        join,
        selection,
        group_by,
        order_by,
        limit,
        offset,
        lock,
        with: None,
        text,
    })
}

fn lower_from(from: &[TableWithJoins]) -> Result<(Option<TableName>, Option<Join>)> {
    if from.is_empty() {
        return Ok((None, None));
    }
    // F09 gap / completeness wave: comma joins and n-table joins.
    if from.len() > 1 {
        return Err(Error::unsupported("comma joins"));
    }
    let table = &from[0];
    let left = table_factor_name(&table.relation)?;
    if table.joins.is_empty() {
        return Ok((Some(left), None));
    }
    if table.joins.len() != 1 {
        return Err(Error::unsupported("more than two tables in JOIN"));
    }
    let join = &table.joins[0];
    let right = table_factor_name(&join.relation)?;
    let (tp, on) = match &join.join_operator {
        JoinOperator::Join(JoinConstraint::On(expr))
        | JoinOperator::Inner(JoinConstraint::On(expr)) => (JoinType::Inner, lower_expr(expr)?),
        JoinOperator::Left(JoinConstraint::On(expr))
        | JoinOperator::LeftOuter(JoinConstraint::On(expr)) => {
            (JoinType::LeftOuter, lower_expr(expr)?)
        }
        JoinOperator::Right(JoinConstraint::On(expr))
        | JoinOperator::RightOuter(JoinConstraint::On(expr)) => {
            (JoinType::RightOuter, lower_expr(expr)?)
        }
        // F09 gap / completeness wave: CROSS / NATURAL / USING / semi / anti / full.
        other => return Err(Error::unsupported(format!("JOIN {other:?}"))),
    };
    Ok((
        Some(left.clone()),
        Some(Join {
            left,
            right,
            tp,
            on,
        }),
    ))
}

fn table_factor_name(factor: &TableFactor) -> Result<TableName> {
    match factor {
        TableFactor::Table { name, .. } => object_table_name(name),
        other => Err(Error::unsupported(format!("FROM {other}"))),
    }
}

fn lower_group_by(group_by: &GroupByExpr) -> Result<Vec<ExprNode>> {
    match group_by {
        GroupByExpr::Expressions(exprs, modifiers) => {
            // F08 gap / completeness wave: ROLLUP / CUBE / GROUPING SETS.
            if !modifiers.is_empty() {
                return Err(Error::unsupported("GROUP BY WITH ROLLUP/CUBE"));
            }
            exprs.iter().map(lower_expr).collect()
        }
        GroupByExpr::All(_) => Err(Error::unsupported("GROUP BY ALL")),
    }
}

fn lower_order_by(order_by: Option<&sqlparser::ast::OrderBy>) -> Result<Vec<ByItem>> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    match &order_by.kind {
        OrderByKind::Expressions(items) => items
            .iter()
            .map(|item| {
                Ok(ByItem {
                    expr: lower_expr(&item.expr)?,
                    desc: matches!(item.options.sort, Some(OrderBySort::Desc)),
                })
            })
            .collect(),
        other => Err(Error::unsupported(format!("ORDER BY {other:?}"))),
    }
}

fn lower_limit(limit: Option<&LimitClause>) -> Result<(Option<u64>, Option<u64>)> {
    match limit {
        None => Ok((None, None)),
        Some(LimitClause::LimitOffset { limit, offset, .. }) => {
            let limit = limit.as_ref().map(expr_as_u64).transpose()?;
            let offset = offset.as_ref().map(|o| expr_as_u64(&o.value)).transpose()?;
            Ok((limit, offset))
        }
        Some(LimitClause::OffsetCommaLimit { offset, limit }) => {
            Ok((Some(expr_as_u64(limit)?), Some(expr_as_u64(offset)?)))
        }
    }
}

fn expr_as_u64(expr: &Expr) -> Result<u64> {
    match expr {
        Expr::Value(v) => match &v.value {
            Value::Number(n, _) => n
                .parse::<u64>()
                .map_err(|e| Error::new(ERR_PARSE_ERROR, e.to_string())),
            other => Err(Error::unsupported(format!("limit literal {other}"))),
        },
        other => Err(Error::unsupported(format!("limit expression {other}"))),
    }
}

fn lower_update(update: &Update) -> Result<Statement> {
    // F07 gaps / completeness wave: UPDATE FROM, RETURNING, ORDER BY, LIMIT, multi-table.
    if update.from.is_some() || update.returning.is_some() || update.output.is_some() {
        return Err(Error::unsupported("UPDATE FROM / RETURNING"));
    }
    if !update.order_by.is_empty() || update.limit.is_some() {
        return Err(Error::unsupported("UPDATE ORDER BY / LIMIT"));
    }
    let table = table_from_relation(&update.table)?;
    let list = update
        .assignments
        .iter()
        .map(|item| {
            let column = match &item.target {
                AssignmentTarget::ColumnName(name) => object_name_last(name)?,
                AssignmentTarget::Tuple(_) => {
                    return Err(Error::unsupported("tuple UPDATE assignment"));
                }
            };
            Ok(Assignment {
                column,
                expr: lower_expr(&item.value)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if list.is_empty() {
        return Err(Error::unsupported("UPDATE without SET"));
    }
    Ok(Statement::Update(UpdateStmt {
        table,
        list,
        selection: update.selection.as_ref().map(lower_expr).transpose()?,
    }))
}

fn lower_delete(delete: &Delete) -> Result<Statement> {
    // F07 gaps / completeness wave: multi-table, USING, RETURNING, ORDER BY, LIMIT.
    if delete.using.is_some()
        || delete.returning.is_some()
        || delete.output.is_some()
        || !delete.tables.is_empty()
    {
        return Err(Error::unsupported("multi-table / RETURNING DELETE"));
    }
    if !delete.order_by.is_empty() || delete.limit.is_some() {
        return Err(Error::unsupported("DELETE ORDER BY / LIMIT"));
    }
    let tables = match &delete.from {
        FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables) => tables,
    };
    if tables.len() != 1 {
        return Err(Error::unsupported("multi-table DELETE"));
    }
    Ok(Statement::Delete(DeleteStmt {
        table: table_from_relation(&tables[0])?,
        selection: delete.selection.as_ref().map(lower_expr).transpose()?,
    }))
}

fn table_from_relation(table: &TableWithJoins) -> Result<TableName> {
    if !table.joins.is_empty() {
        return Err(Error::unsupported("JOIN in DML"));
    }
    match &table.relation {
        TableFactor::Table { name, .. } => object_table_name(name),
        other => Err(Error::unsupported(format!("DML target {other}"))),
    }
}

fn lower_values(values: &Values) -> Result<Vec<Vec<crate::ast::expressions::ExprNode>>> {
    values
        .rows
        .iter()
        .map(|row| row.iter().map(lower_expr).collect())
        .collect()
}

fn object_table_name(name: &ObjectName) -> Result<TableName> {
    let parts = object_name_parts(name)?;
    match parts.as_slice() {
        [table] => Ok(TableName::new(None, table.clone())),
        [schema, table] => Ok(TableName::new(Some(schema.clone()), table.clone())),
        _ => Err(Error::unsupported(format!("object name {name}"))),
    }
}

fn object_name_last(name: &ObjectName) -> Result<String> {
    object_name_parts(name)?
        .pop()
        .ok_or_else(|| Error::new(ERR_PARSE_ERROR, "empty object name"))
}

fn schema_name_last(name: &SchemaName) -> Result<String> {
    match name {
        SchemaName::Simple(name) => object_name_last(name),
        other => Err(Error::unsupported(format!("schema name {other}"))),
    }
}

fn object_name_parts(name: &ObjectName) -> Result<Vec<String>> {
    name.0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(|ident| ident.value.clone())
                .ok_or_else(|| Error::unsupported(format!("object name part {part}")))
        })
        .collect()
}

fn lower_select_item(item: &SelectItem) -> Result<SelectField> {
    match item {
        SelectItem::UnnamedExpr(expr) => Ok(SelectField {
            expr: lower_expr(expr)?,
            as_name: None,
        }),
        SelectItem::ExprWithAlias { expr, alias } => Ok(SelectField {
            expr: lower_expr(expr)?,
            as_name: Some(alias.value.clone()),
        }),
        SelectItem::ExprWithAliases { expr, aliases } => Ok(SelectField {
            expr: lower_expr(expr)?,
            as_name: aliases.first().map(|a| a.value.clone()),
        }),
        SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => Ok(SelectField {
            expr: ExprNode::Value(ValueExpr {
                datum: Datum::Bytes(b"*".to_vec()),
            }),
            as_name: None,
        }),
    }
}

fn lower_expr(expr: &Expr) -> Result<ExprNode> {
    match expr {
        Expr::Value(v) => Ok(ExprNode::Value(ValueExpr {
            datum: lower_value(&v.value)?,
        })),
        Expr::BinaryOp { left, op, right } => Ok(ExprNode::Binary(BinaryOpExpr {
            op: lower_binary_op(op)?,
            left: Box::new(lower_expr(left)?),
            right: Box::new(lower_expr(right)?),
        })),
        Expr::UnaryOp { op, expr } => Ok(ExprNode::Unary(UnaryOpExpr {
            op: lower_unary_op(op)?,
            expr: Box::new(lower_expr(expr)?),
        })),
        Expr::Nested(inner) => lower_expr(inner),
        Expr::Cast {
            expr, data_type, ..
        } => Ok(ExprNode::Cast(FuncCastExpr {
            expr: Box::new(lower_expr(expr)?),
            target: data_type_name(data_type),
        })),
        Expr::Function(func) => lower_function(func),
        Expr::Identifier(ident) if ident.value.eq_ignore_ascii_case("null") => {
            Ok(ExprNode::Value(ValueExpr { datum: Datum::Null }))
        }
        Expr::Identifier(ident) => Ok(ExprNode::Column(crate::ast::expressions::ColumnNameExpr {
            schema: None,
            table: None,
            name: ident.value.clone(),
        })),
        Expr::CompoundIdentifier(parts) => {
            let names: Vec<String> = parts.iter().map(|p| p.value.clone()).collect();
            let (schema, table, name) = match names.as_slice() {
                [name] => (None, None, name.clone()),
                [table, name] => (None, Some(table.clone()), name.clone()),
                [schema, table, name] => (Some(schema.clone()), Some(table.clone()), name.clone()),
                _ => return Err(Error::unsupported(format!("column name {expr}"))),
            };
            Ok(ExprNode::Column(crate::ast::expressions::ColumnNameExpr {
                schema,
                table,
                name,
            }))
        }
        Expr::Exists { subquery, negated } => {
            let query = lower_query(subquery)?;
            if query.join.is_some() {
                return Err(Error::unsupported("subquery JOIN"));
            }
            Ok(ExprNode::Exists(ExistsExpr {
                not: *negated,
                query: Box::new(query),
            }))
        }
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => {
            let query = lower_query(subquery)?;
            if query.join.is_some() {
                return Err(Error::unsupported("subquery JOIN"));
            }
            Ok(ExprNode::InSubquery(InSubqueryExpr {
                expr: Box::new(lower_expr(expr)?),
                not: *negated,
                query: Box::new(query),
            }))
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => Ok(ExprNode::InList(InListExpr {
            expr: Box::new(lower_expr(expr)?),
            not: *negated,
            list: list.iter().map(lower_expr).collect::<Result<Vec<_>>>()?,
        })),
        Expr::Subquery(subquery) => {
            let query = lower_query(subquery)?;
            if query.join.is_some() {
                return Err(Error::unsupported("subquery JOIN"));
            }
            Ok(ExprNode::ScalarSubquery(ScalarSubqueryExpr {
                query: Box::new(query),
            }))
        }
        // F14 gap / completeness wave: ANY / ALL / SOME, FROM derived tables.
        Expr::AnyOp { .. } => Err(Error::unsupported("ANY / SOME subquery")),
        Expr::AllOp { .. } => Err(Error::unsupported("ALL subquery")),
        other => Err(Error::new(
            ERR_UNSUPPORTED,
            format!("expression {other} is not available in this phase"),
        )),
    }
}

fn lower_function(func: &Function) -> Result<ExprNode> {
    if let Some(over) = &func.over {
        return lower_window_func(func, over);
    }
    let name = func.name.to_string();
    let args = match &func.args {
        FunctionArguments::List(list) => list
            .args
            .iter()
            .map(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                } => lower_expr(expr),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                | FunctionArg::Unnamed(FunctionArgExpr::WildcardWithOptions(_)) => {
                    Ok(ExprNode::Value(ValueExpr {
                        datum: Datum::Bytes(b"*".to_vec()),
                    }))
                }
                _ => Err(Error::unsupported("function argument form")),
            })
            .collect::<Result<Vec<_>>>()?,
        FunctionArguments::None => Vec::new(),
        FunctionArguments::Subquery(_) => {
            return Err(Error::unsupported("function subquery argument"));
        }
    };
    let args = if name.eq_ignore_ascii_case("COUNT")
        && matches!(
            args.as_slice(),
            [ExprNode::Value(v)] if matches!(&v.datum, Datum::Bytes(b) if b == b"*")
        ) {
        Vec::new()
    } else {
        args
    };
    Ok(ExprNode::Func(FuncCallExpr { name, args }))
}

fn lower_window_func(func: &Function, over: &WindowType) -> Result<ExprNode> {
    // F16 gap / completeness wave: named windows, frame clauses, LAG/LEAD.
    if func.null_treatment.is_some() {
        return Err(Error::unsupported("IGNORE/RESPECT NULLS"));
    }
    let WindowType::WindowSpec(spec) = over else {
        return Err(Error::unsupported("named window"));
    };
    if spec.window_name.is_some() || spec.window_frame.is_some() {
        return Err(Error::unsupported("window name / frame clause"));
    }
    let name = func.name.to_string();
    match name.to_ascii_uppercase().as_str() {
        "ROW_NUMBER" | "RANK" | "DENSE_RANK" | "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" => {}
        other => return Err(Error::unsupported(format!("window function {other}"))),
    }
    let args = match &func.args {
        FunctionArguments::List(list) => list
            .args
            .iter()
            .map(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr))
                | FunctionArg::Named {
                    arg: FunctionArgExpr::Expr(expr),
                    ..
                } => lower_expr(expr),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard)
                | FunctionArg::Unnamed(FunctionArgExpr::WildcardWithOptions(_)) => {
                    Ok(ExprNode::Value(ValueExpr {
                        datum: Datum::Bytes(b"*".to_vec()),
                    }))
                }
                _ => Err(Error::unsupported("window function argument form")),
            })
            .collect::<Result<Vec<_>>>()?,
        FunctionArguments::None => Vec::new(),
        FunctionArguments::Subquery(_) => {
            return Err(Error::unsupported("window function subquery argument"));
        }
    };
    let args = if name.eq_ignore_ascii_case("COUNT")
        && matches!(
            args.as_slice(),
            [ExprNode::Value(v)] if matches!(&v.datum, Datum::Bytes(b) if b == b"*")
        ) {
        Vec::new()
    } else {
        args
    };
    Ok(ExprNode::Window(WindowFuncExpr {
        name,
        args,
        spec: WindowSpec {
            partition_by: spec
                .partition_by
                .iter()
                .map(lower_expr)
                .collect::<Result<Vec<_>>>()?,
            order_by: lower_order_by_exprs(&spec.order_by)?,
        },
    }))
}

fn lower_order_by_exprs(items: &[OrderByExpr]) -> Result<Vec<ByItem>> {
    items
        .iter()
        .map(|item| {
            Ok(ByItem {
                expr: lower_expr(&item.expr)?,
                desc: matches!(item.options.sort, Some(OrderBySort::Desc)),
            })
        })
        .collect()
}

fn lower_value(value: &Value) -> Result<Datum> {
    match value {
        Value::Number(n, _) => {
            if n.contains('.') {
                n.parse::<f64>()
                    .map(Datum::F64)
                    .map_err(|e| Error::new(ERR_PARSE_ERROR, e.to_string()))
            } else {
                n.parse::<i64>()
                    .map(Datum::I64)
                    .map_err(|e| Error::new(ERR_PARSE_ERROR, e.to_string()))
            }
        }
        Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) => {
            Ok(Datum::Bytes(s.as_bytes().to_vec()))
        }
        Value::Boolean(b) => Ok(Datum::I64(i64::from(*b))),
        Value::Null => Ok(Datum::Null),
        other => Err(Error::unsupported(format!("literal {other}"))),
    }
}

fn lower_binary_op(op: &BinaryOperator) -> Result<Op> {
    Ok(match op {
        BinaryOperator::Plus => Op::Plus,
        BinaryOperator::Minus => Op::Minus,
        BinaryOperator::Multiply => Op::Mul,
        BinaryOperator::Divide => Op::Div,
        BinaryOperator::Modulo => Op::Mod,
        BinaryOperator::Eq => Op::Eq,
        BinaryOperator::NotEq => Op::Ne,
        BinaryOperator::Lt => Op::Lt,
        BinaryOperator::LtEq => Op::Le,
        BinaryOperator::Gt => Op::Gt,
        BinaryOperator::GtEq => Op::Ge,
        BinaryOperator::And => Op::LogicAnd,
        BinaryOperator::Or => Op::LogicOr,
        _ => return Err(Error::unsupported(format!("operator {op}"))),
    })
}

fn lower_unary_op(op: &UnaryOperator) -> Result<Op> {
    Ok(match op {
        UnaryOperator::Plus => Op::Plus,
        UnaryOperator::Minus => Op::UnaryMinus,
        UnaryOperator::Not => Op::Not,
        _ => return Err(Error::unsupported(format!("unary operator {op}"))),
    })
}

fn lower_create_index(create: &sqlparser::ast::CreateIndex) -> Result<Statement> {
    // F18 gap / completeness wave: UNIQUE, USING, partial, invisible, ALGORITHM/LOCK.
    if create.unique {
        return Err(Error::unsupported("UNIQUE INDEX"));
    }
    if create.using.is_some()
        || !create.include.is_empty()
        || create.predicate.is_some()
        || !create.with.is_empty()
        || !create.alter_options.is_empty()
        || create.concurrently
    {
        return Err(Error::unsupported("CREATE INDEX option"));
    }
    let columns = create
        .columns
        .iter()
        .map(index_column_name)
        .collect::<Result<Vec<_>>>()?;
    if columns.is_empty() {
        return Err(Error::unsupported("CREATE INDEX without columns"));
    }
    let name = match &create.name {
        Some(name) => object_name_last(name)?,
        None => format!("idx_{}", columns.join("_")),
    };
    Ok(Statement::CreateIndex(CreateIndexStmt {
        if_not_exists: create.if_not_exists,
        table: object_table_name(&create.table_name)?,
        name,
        columns,
    }))
}

fn lower_alter_table(alter: &sqlparser::ast::AlterTable) -> Result<Statement> {
    // F18 gap / completeness wave: CHANGE/MODIFY/RENAME, multi-spec mix beyond
    // ADD/DROP COLUMN|INDEX, partitions, AFTER/FIRST, generated columns.
    if alter.if_exists || alter.only || alter.location.is_some() || alter.on_cluster.is_some() {
        return Err(Error::unsupported("ALTER TABLE option"));
    }
    if alter.operations.is_empty() {
        return Err(Error::unsupported("ALTER TABLE without operation"));
    }
    let specs = alter
        .operations
        .iter()
        .map(lower_alter_spec)
        .collect::<Result<Vec<_>>>()?;
    Ok(Statement::AlterTable(AlterTableStmt {
        table: object_table_name(&alter.name)?,
        specs,
    }))
}

fn lower_alter_spec(op: &AlterTableOperation) -> Result<AlterTableSpec> {
    match op {
        AlterTableOperation::AddColumn {
            if_not_exists,
            column_def,
            column_position,
            ..
        } => {
            if column_position.is_some() {
                return Err(Error::unsupported("ALTER TABLE ADD COLUMN AFTER/FIRST"));
            }
            Ok(AlterTableSpec::AddColumn {
                if_not_exists: *if_not_exists,
                column: ColumnDef {
                    name: column_def.name.value.clone(),
                    type_name: column_def.data_type.to_string(),
                },
            })
        }
        AlterTableOperation::DropColumn {
            column_names,
            if_exists,
            ..
        } => {
            if column_names.len() != 1 {
                return Err(Error::unsupported("ALTER TABLE DROP multiple columns"));
            }
            Ok(AlterTableSpec::DropColumn {
                if_exists: *if_exists,
                name: column_names[0].value.clone(),
            })
        }
        AlterTableOperation::DropIndex { name } => Ok(AlterTableSpec::DropIndex {
            if_exists: false,
            name: name.value.clone(),
        }),
        AlterTableOperation::AddConstraint { constraint, .. } => match constraint {
            TableConstraint::Index(idx) => {
                let columns = idx
                    .columns
                    .iter()
                    .map(index_column_name)
                    .collect::<Result<Vec<_>>>()?;
                if columns.is_empty() {
                    return Err(Error::unsupported("INDEX without columns"));
                }
                if idx.index_type.is_some() {
                    return Err(Error::unsupported("INDEX USING type"));
                }
                let name = idx
                    .name
                    .as_ref()
                    .map(|ident| ident.value.clone())
                    .unwrap_or_else(|| format!("idx_{}", columns.join("_")));
                Ok(AlterTableSpec::AddIndex(IndexDef { name, columns }))
            }
            other => Err(Error::unsupported(format!("ALTER TABLE ADD {other}"))),
        },
        other => Err(Error::unsupported(format!("ALTER TABLE {other}"))),
    }
}

fn lower_table_indices(constraints: &[TableConstraint]) -> Result<Vec<IndexDef>> {
    let mut indices = Vec::new();
    for constraint in constraints {
        match constraint {
            TableConstraint::Index(idx) => {
                let columns = idx
                    .columns
                    .iter()
                    .map(index_column_name)
                    .collect::<Result<Vec<_>>>()?;
                if columns.is_empty() {
                    return Err(Error::unsupported("INDEX without columns"));
                }
                // F10 gap / completeness wave: prefix length, functional, USING HASH.
                if idx.index_type.is_some() {
                    return Err(Error::unsupported("INDEX USING type"));
                }
                let name = idx
                    .name
                    .as_ref()
                    .map(|ident| ident.value.clone())
                    .unwrap_or_else(|| format!("idx_{}", columns.join("_")));
                indices.push(IndexDef { name, columns });
            }
            // F10/F18 gaps: UNIQUE, PRIMARY KEY, FK, CHECK, FULLTEXT stay deferred.
            TableConstraint::Unique(_) => return Err(Error::unsupported("UNIQUE constraint")),
            TableConstraint::PrimaryKey(_) => return Err(Error::unsupported("PRIMARY KEY")),
            TableConstraint::ForeignKey(_) => {
                return Err(Error::unsupported("FOREIGN KEY"));
            }
            TableConstraint::Check(_) => return Err(Error::unsupported("CHECK constraint")),
            TableConstraint::FulltextOrSpatial(_) => {
                return Err(Error::unsupported("FULLTEXT / SPATIAL INDEX"));
            }
            other => {
                return Err(Error::unsupported(format!(
                    "CREATE TABLE constraint {other}"
                )));
            }
        }
    }
    Ok(indices)
}

fn index_column_name(col: &sqlparser::ast::IndexColumn) -> Result<String> {
    match &col.column.expr {
        Expr::Identifier(ident) => Ok(ident.value.clone()),
        other => Err(Error::unsupported(format!("INDEX column {other}"))),
    }
}

fn data_type_name(tp: &DataType) -> String {
    match tp {
        DataType::Int(_) | DataType::Integer(_) | DataType::BigInt(_) | DataType::TinyInt(_) => {
            "SIGNED".to_string()
        }
        DataType::IntUnsigned(_)
        | DataType::IntegerUnsigned(_)
        | DataType::BigIntUnsigned(_)
        | DataType::TinyIntUnsigned(_)
        | DataType::Unsigned
        | DataType::UnsignedInteger => "UNSIGNED".to_string(),
        DataType::Double(_) | DataType::Float(_) | DataType::Real => "DOUBLE".to_string(),
        DataType::Decimal(_) | DataType::Numeric(_) => "DECIMAL".to_string(),
        DataType::Char(_) | DataType::Varchar(_) | DataType::Text | DataType::String(_) => {
            "CHAR".to_string()
        }
        other => other.to_string(),
    }
}
