use crate::create_table::{build_table_info, columns_from_select};
use crate::table::resolve_name;
use sealdb_errno::{
    ERR_BAD_DB, ERR_DEFINER_TOO_LONG, ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS, ERR_VIEW_INVALID,
    ERR_VIEW_INVALID_TABLE, ERR_VIEW_SELECT_VARIABLE, ERR_VIEW_WRONG_LIST, ERR_WRONG_OBJECT, Error,
    Result,
};
use sealdb_infoschema::InfoSchema;
use sealdb_meta::{Mutator, TableInfo, ViewInfo};
use sealdb_parser::Parser;
use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode};
use sealdb_parser::ast::{
    CreateTableStmt, CreateViewStmt, SelectField, SelectStmt, SetOprStmt, Statement,
};
use sealdb_parser::format::RestoreCtx;
use sealdb_parser::model::CIStr;
use std::collections::HashMap;

/// Whether a restored SELECT body references a variable (`@v`, `@@global.x`)
/// outside string literals. Used for the 1351 VIEW check.
fn sql_has_variable(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // Only string literals hide variables; backticked identifiers are
            // how the bridge renders `@@global.x` (`` `@@global`.`x` ``).
            b'\'' | b'"' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            b'@' => return true,
            _ => i += 1,
        }
    }
    false
}

/// Walk the tables referenced by a view body and reject missing tables — and
/// views whose own bodies no longer resolve — with 1146. TiDB materializes the
/// whole view chain, so a nested invalid view fails the outer CREATE too.
fn validate_select_references(
    is: &InfoSchema,
    select: &SelectStmt,
    current_db: &str,
    excluded: Option<(&str, &str)>,
    chain: &mut Vec<String>,
) -> Result<()> {
    let cte_names: Vec<String> = select
        .with
        .as_ref()
        .map(|with| with.ctes.iter().map(|cte| cte.name.clone()).collect())
        .unwrap_or_default();
    for table in select.join_tables() {
        let (schema, name) = table.qualified(current_db);
        if schema.is_empty() {
            continue;
        }
        if cte_names.iter().any(|c| c.eq_ignore_ascii_case(&name)) {
            continue;
        }
        if excluded.is_some_and(|(es, en)| {
            es.eq_ignore_ascii_case(&schema) && en.eq_ignore_ascii_case(&name)
        }) {
            return Err(Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{schema}.{name}' doesn't exist"),
            ));
        }
        let Some(info) = is.table_by_name(&schema, &name) else {
            return Err(Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{schema}.{name}' doesn't exist"),
            ));
        };
        let Some(view) = &info.view else {
            continue;
        };
        let key = format!(
            "{}.{}",
            schema.to_ascii_lowercase(),
            name.to_ascii_lowercase()
        );
        if chain.len() >= 64 || chain.iter().any(|seen| seen == &key) {
            continue;
        }
        let parsed = Parser::new().parse_one_stmt(&view.select_stmt)?;
        chain.push(key);
        let result = match &parsed {
            Statement::Select(inner) => {
                validate_select_references(is, inner, &schema, excluded, chain)
            }
            Statement::SetOpr(set) => validate_set_references(is, set, &schema, excluded, chain),
            _ => Ok(()),
        };
        chain.pop();
        result?;
    }
    Ok(())
}

fn validate_set_references(
    is: &InfoSchema,
    set: &SetOprStmt,
    current_db: &str,
    excluded: Option<(&str, &str)>,
    chain: &mut Vec<String>,
) -> Result<()> {
    validate_select_references(is, &set.first, current_db, excluded, chain)?;
    for (_, arm) in &set.rest {
        validate_select_references(is, arm, current_db, excluded, chain)?;
    }
    Ok(())
}

fn validate_view_references(
    is: &InfoSchema,
    stmt: &CreateViewStmt,
    current_db: &str,
    excluded: Option<(&str, &str)>,
) -> Result<()> {
    let mut chain = Vec::new();
    validate_select_references(is, &stmt.select, current_db, excluded, &mut chain)?;
    if let Some(set) = &stmt.select_set {
        for (_, arm) in &set.rest {
            validate_select_references(is, arm, current_db, excluded, &mut chain)?;
        }
    }
    Ok(())
}

/// Reject view definers whose user/host exceed MySQL limits (1470).
fn validate_definer(definer: &str) -> Result<()> {
    if definer.eq_ignore_ascii_case("CURRENT_USER") {
        return Ok(());
    }
    let plain = definer.replace('`', "");
    let Some((user, host)) = plain.split_once('@') else {
        return Ok(());
    };
    let (user_len, host_len) = (user.chars().count(), host.chars().count());
    // TiDB `ErrWrongStringLength`: `String '%.70s' is too long for <part>`.
    if user_len > 32 {
        return Err(Error::new(
            ERR_DEFINER_TOO_LONG,
            format!(
                "String '{}' is too long for user name (should be no longer than 32)",
                user.chars().take(70).collect::<String>()
            ),
        ));
    }
    if host_len > 255 {
        return Err(Error::new(
            ERR_DEFINER_TOO_LONG,
            format!(
                "String '{}' is too long for host name (should be no longer than 255)",
                host.chars().take(70).collect::<String>()
            ),
        ));
    }
    Ok(())
}

/// Does the view body read a table `AS OF TIMESTAMP` (stale read)?
fn select_has_stale_read(select: &SelectStmt) -> bool {
    if select.join_tables().iter().any(|t| t.as_of_ts.is_some()) {
        return true;
    }
    if let Some(with) = &select.with {
        for cte in &with.ctes {
            if select_has_stale_read(&cte.query)
                || cte
                    .set_rest
                    .iter()
                    .any(|(_, arm)| select_has_stale_read(arm))
            {
                return true;
            }
        }
    }
    false
}

/// One `FROM` source used when expanding `*` in a view body.
struct ViewSource {
    /// Qualifier written in front of expanded columns.
    schema: Option<String>,
    table: Option<String>,
    /// Names a qualified star may use to reference this source.
    names: Vec<String>,
    columns: Vec<String>,
}

fn view_sources(
    is: &InfoSchema,
    select: &SelectStmt,
    current_db: &str,
    scope: &HashMap<String, Vec<String>>,
) -> Result<Vec<ViewSource>> {
    let mut sources = Vec::new();
    for table in select.join_tables() {
        let physical = table.resolve_name();
        if let Some(cols) = scope.get(&physical.to_ascii_lowercase()) {
            sources.push(ViewSource {
                schema: None,
                table: Some(table.name.clone()),
                names: vec![table.name.clone()],
                columns: cols.clone(),
            });
            continue;
        }
        let (schema, name) = table.qualified(current_db);
        let info = is.table_by_name(&schema, &name).ok_or_else(|| {
            Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{schema}.{name}' doesn't exist"),
            )
        })?;
        let columns = info
            .columns
            .iter()
            .map(|col| col.name.o().to_string())
            .collect();
        if table.physical.is_some() {
            sources.push(ViewSource {
                schema: None,
                table: Some(table.name.clone()),
                names: vec![table.name.clone()],
                columns,
            });
        } else {
            sources.push(ViewSource {
                schema: Some(schema),
                table: Some(name.clone()),
                names: vec![table.name.clone(), name],
                columns,
            });
        }
    }
    Ok(sources)
}

/// Expand `*` / `t.*` into an explicit, schema-qualified select list the way
/// TiDB stores a view definition.
fn expand_view_stars(is: &InfoSchema, select: &mut SelectStmt, current_db: &str) -> Result<()> {
    expand_view_stars_scoped(is, select, current_db, &HashMap::new())
}

fn expand_view_stars_scoped(
    is: &InfoSchema,
    select: &mut SelectStmt,
    current_db: &str,
    outer: &HashMap<String, Vec<String>>,
) -> Result<()> {
    let mut scope = outer.clone();
    if let Some(with) = &mut select.with {
        for cte in with.ctes.iter_mut() {
            expand_view_stars_scoped(is, &mut cte.query, current_db, &scope)?;
            for (_, arm) in cte.set_rest.iter_mut() {
                expand_view_stars_scoped(is, arm, current_db, &scope)?;
            }
            if let Some((_, rec)) = &mut cte.recursive {
                expand_view_stars_scoped(is, rec, current_db, &scope)?;
            }
            for (_, rec) in cte.recursive_rest.iter_mut() {
                expand_view_stars_scoped(is, rec, current_db, &scope)?;
            }
            let mut names: Vec<String> = cte
                .query
                .fields
                .iter()
                .enumerate()
                .map(|(i, field)| crate::create_table::select_field_name(field, i))
                .collect();
            if !cte.col_name_list.is_empty() && cte.col_name_list.len() == names.len() {
                names = cte.col_name_list.clone();
            }
            scope.insert(cte.name.to_ascii_lowercase(), names);
        }
    }
    let sources = view_sources(is, select, current_db, &scope)?;
    let mut fields = Vec::with_capacity(select.fields.len());
    for field in &select.fields {
        if field.expr.is_star_marker() {
            for source in &sources {
                push_star_fields(&mut fields, source, source.schema.clone())?;
            }
            continue;
        }
        if let ExprNode::Column(col) = &field.expr
            && col.name == "*"
        {
            let hint = col.table.as_deref().unwrap_or_default();
            let source = sources
                .iter()
                .find(|source| {
                    source
                        .names
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(hint))
                })
                .ok_or_else(|| {
                    Error::new(ERR_NO_SUCH_TABLE, format!("Table '{hint}' doesn't exist"))
                })?;
            let schema = col.schema.clone().or_else(|| source.schema.clone());
            push_star_fields(&mut fields, source, schema)?;
            continue;
        }
        fields.push(field.clone());
    }
    select.fields = fields;
    Ok(())
}

fn push_star_fields(
    fields: &mut Vec<SelectField>,
    source: &ViewSource,
    schema: Option<String>,
) -> Result<()> {
    for name in &source.columns {
        fields.push(SelectField {
            expr: ExprNode::Column(ColumnNameExpr {
                schema: schema.clone(),
                table: source.table.clone(),
                name: name.clone(),
            }),
            as_name: Some(name.clone()),
            orig_text: None,
        });
    }
    Ok(())
}

fn expand_set_view_stars(is: &InfoSchema, set: &mut SetOprStmt, current_db: &str) -> Result<()> {
    let mut scope = HashMap::new();
    if let Some(with) = &mut set.with {
        for cte in with.ctes.iter_mut() {
            expand_view_stars_scoped(is, &mut cte.query, current_db, &scope)?;
            for (_, arm) in cte.set_rest.iter_mut() {
                expand_view_stars_scoped(is, arm, current_db, &scope)?;
            }
            let mut names: Vec<String> = cte
                .query
                .fields
                .iter()
                .enumerate()
                .map(|(i, field)| crate::create_table::select_field_name(field, i))
                .collect();
            if !cte.col_name_list.is_empty() && cte.col_name_list.len() == names.len() {
                names = cte.col_name_list.clone();
            }
            scope.insert(cte.name.to_ascii_lowercase(), names);
        }
    }
    expand_view_stars_scoped(is, &mut set.first, current_db, &scope)?;
    for (_, arm) in set.rest.iter_mut() {
        expand_view_stars_scoped(is, arm, current_db, &scope)?;
    }
    Ok(())
}

pub fn create_view(
    meta: &mut Mutator,
    is: &InfoSchema,
    stmt: &CreateViewStmt,
    current_db: &str,
) -> Result<TableInfo> {
    let (schema, name) = resolve_name(&stmt.view_name, current_db)?;
    let db = is
        .schema_by_name(&schema)
        .ok_or_else(|| Error::new(ERR_BAD_DB, format!("Unknown database '{schema}'")))?;
    let mut replace_existing = None;
    if let Some(existing) = is.table_by_name(&schema, &name) {
        if stmt.or_replace {
            if !existing.is_view() {
                return Err(Error::new(
                    ERR_WRONG_OBJECT,
                    format!("'{schema}.{name}' is not VIEW"),
                ));
            }
            replace_existing = Some(existing.id);
        } else {
            return Err(Error::new(
                ERR_TABLE_EXISTS,
                format!("Table '{schema}.{name}' already exists"),
            ));
        }
    }
    // Validate the body before dropping the replaced view so a failing
    // CREATE OR REPLACE leaves the old definition intact. The replaced view is
    // treated as absent while walking the dependency chain, matching TiDB.
    validate_definer(&stmt.definer)?;
    let excluded = stmt.or_replace.then_some((schema.as_str(), name.as_str()));
    validate_view_references(is, stmt, current_db, excluded)?;
    if let Some(id) = replace_existing {
        meta.drop_table_or_view(db.id, id)?;
    }
    let check = stmt.check_option.trim();
    if stmt.algorithm.eq_ignore_ascii_case("TEMPTABLE")
        && !check.is_empty()
        && !check.eq_ignore_ascii_case("NONE")
    {
        return Err(Error::new(
            ERR_VIEW_INVALID,
            format!(
                "VIEW '{}' WITH CHECK OPTION is invalid for ALGORITHM=TEMPTABLE",
                name
            ),
        ));
    }
    let table_id = meta.gen_global_id()?;
    // Persist the definition in TiDB's `SHOW CREATE VIEW` form: `*` expanded
    // with schema-qualified names, every field aliased, derived tables kept.
    let mut view_select = stmt.select.clone();
    expand_view_stars(is, &mut view_select, current_db)?;
    let mut view_set = stmt.select_set.as_deref().cloned();
    if let Some(set) = &mut view_set {
        expand_set_view_stars(is, set, current_db)?;
    }
    let mut restore = RestoreCtx::new_view(&schema);
    match &view_set {
        Some(set) => set.restore(&mut restore)?,
        None => view_select.restore(&mut restore)?,
    }
    let select_text = restore.into_string();
    // TiDB rejects any variable / parameter in the view body (1351).
    if sql_has_variable(&select_text) {
        return Err(Error::new(
            ERR_VIEW_SELECT_VARIABLE,
            "View's SELECT contains a variable or parameter",
        ));
    }
    // TiDB forbids stale reads (`AS OF TIMESTAMP`) in a view definition (1356).
    if select_has_stale_read(&stmt.select)
        || stmt
            .select_set
            .as_ref()
            .is_some_and(|set| set.rest.iter().any(|(_, arm)| select_has_stale_read(arm)))
    {
        return Err(Error::new(
            ERR_VIEW_INVALID_TABLE,
            format!(
                "View '{schema}.{name}' references invalid table(s) or column(s) \
                 or function(s) or definer/invoker of view lack rights to use them"
            ),
        ));
    }
    let mut cols = columns_from_select(is, &stmt.select, current_db)?;
    if let Some(set) = &stmt.select_set {
        // Every UNION arm must resolve like a standalone query so a missing
        // table anywhere in the set operation reports 1146.
        for (_, arm) in &set.rest {
            let _ = columns_from_select(is, arm, current_db)?;
        }
    }
    if !stmt.cols.is_empty() {
        if stmt.cols.len() != cols.len() {
            return Err(Error::new(
                ERR_VIEW_WRONG_LIST,
                "In definition of view, derived table or common table expression, \
                 SELECT list and column names list have different column counts",
            ));
        }
        for (col, rename) in cols.iter_mut().zip(stmt.cols.iter()) {
            col.name = rename.clone();
        }
    }
    let create = CreateTableStmt {
        if_not_exists: false,
        table: stmt.view_name.clone(),
        cols,
        indices: Vec::new(),
        foreign_keys: Vec::new(),
        charset: None,
        collate: None,
        partition: None,
        ttl: None,
        refer_table: None,
        select: None,
    };
    let mut info = build_table_info(&create, table_id)?;
    info.view = Some(ViewInfo {
        algorithm: stmt.algorithm.clone(),
        definer: stmt.definer.clone(),
        security: stmt.security.clone(),
        select_stmt: select_text,
        check_option: stmt.check_option.clone(),
        cols: stmt.cols.iter().map(CIStr::new).collect(),
    });
    meta.create_table_or_view(db.id, &info)?;
    meta.gen_schema_version()?;
    Ok(info)
}
