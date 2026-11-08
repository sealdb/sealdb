//! Foreign-key plan nodes under DML (TiDB `Foreign_Key_Check` /
//! `Foreign_Key_Cascade`).
//!
//! TiDB builds these as physical children of `Insert`, `Update` and `Delete`:
//!
//! * an inserted / updated local FK column must exist in the parent
//!   (`check_exist`, parent table + its covering index);
//! * deleting / updating a referenced key the children still use is blocked
//!   (`check_not_exist`, child table + FK index) unless the FK declares
//!   `ON DELETE CASCADE` / `ON UPDATE CASCADE`, in which case the child rows
//!   are rewritten (`on_delete:CASCADE` / `on_update:CASCADE`).
//!
//! `SET foreign_key_checks = 0` stops the planner from generating them at all.

use sealdb_infoschema::InfoSchema;
use sealdb_meta::{ForeignKeyInfo, TableInfo};
use sealdb_parser::ast::{DeleteStmt, InsertStmt, TableName, UpdateStmt};
use std::cell::Cell;

use super::physical_plans::{FkAction, FkNode};

thread_local! {
    /// TiDB `foreign_key_checks` (default ON). Set per statement by the
    /// session so planning DML need not thread session state through.
    static FOREIGN_KEY_CHECKS: Cell<bool> = const { Cell::new(true) };
}

pub fn foreign_key_checks() -> bool {
    FOREIGN_KEY_CHECKS.with(|cell| cell.get())
}

/// Runs `f` with `foreign_key_checks` overridden, restoring the previous value.
pub fn with_foreign_key_checks<T>(on: bool, f: impl FnOnce() -> T) -> T {
    let previous = FOREIGN_KEY_CHECKS.with(|cell| cell.replace(on));
    let out = f();
    FOREIGN_KEY_CHECKS.with(|cell| cell.set(previous));
    out
}

/// One planned node plus the sort key TiDB's plan builder produces:
/// referenced table, then child table, then FK definition order.
type Keyed = (String, String, usize, FkNode);

pub(crate) fn insert_fk_children(
    insert: &InsertStmt,
    is: &InfoSchema,
    current_db: &str,
) -> Vec<FkNode> {
    if !foreign_key_checks() {
        return Vec::new();
    }
    let Some(target) = resolve_table(is, current_db, &insert.table) else {
        return Vec::new();
    };
    let provided: Vec<String> = if insert.columns.is_empty() {
        target.columns.iter().map(|c| c.name.o().to_string()).collect()
    } else {
        insert.columns.clone()
    };
    let mut checks = Vec::new();
    for (order, fk) in target.foreign_keys.iter().enumerate() {
        if !cols_cover(&fk.cols, &provided) {
            continue;
        }
        if let Some(node) = check_exist_node(is, current_db, target, fk) {
            checks.push((fk.ref_table.clone(), target.name.o().to_string(), order, node));
        }
    }
    // `ON DUPLICATE KEY UPDATE` rewrites columns of an existing row: the same
    // contract checks / cascades a plain UPDATE plans for those columns.
    let updated: Vec<String> = insert
        .on_duplicate
        .iter()
        .map(|assign| assign.column.clone())
        .collect();
    if !insert.on_duplicate.is_empty() {
        for (order, fk) in target.foreign_keys.iter().enumerate() {
            if !cols_cover(&fk.cols, &updated) {
                continue;
            }
            if let Some(node) = check_exist_node(is, current_db, target, fk) {
                checks.push((fk.ref_table.clone(), target.name.o().to_string(), order, node));
            }
        }
    }
    let mut cascades = Vec::new();
    if !insert.on_duplicate.is_empty() {
        let (child_checks, child_cascades) =
            split_by_action(collect_child_fks(is, current_db, target, &updated, true));
        checks.extend(child_checks);
        cascades.extend(child_cascades);
    }
    finish(checks, cascades)
}

pub(crate) fn update_fk_children(
    update: &UpdateStmt,
    is: &InfoSchema,
    current_db: &str,
) -> Vec<FkNode> {
    if !foreign_key_checks() {
        return Vec::new();
    }
    let mut checks = Vec::new();
    let mut cascades = Vec::new();
    for table in affected_tables(is, current_db, &update.table, update.join.as_ref()) {
        let updated: Vec<String> = assignments_for(update, table)
            .into_iter()
            .map(|assign| assign.column.clone())
            .collect();
        if updated.is_empty() {
            continue;
        }
        for (order, fk) in table.foreign_keys.iter().enumerate() {
            if !cols_cover(&fk.cols, &updated) {
                continue;
            }
            if let Some(node) = check_exist_node(is, current_db, table, fk) {
                checks.push((fk.ref_table.clone(), table.name.o().to_string(), order, node));
            }
        }
        collect_child_fks_update(is, current_db, table, &updated, &mut checks, &mut cascades);
    }
    finish(checks, cascades)
}

pub(crate) fn delete_fk_children(
    delete: &DeleteStmt,
    is: &InfoSchema,
    current_db: &str,
) -> Vec<FkNode> {
    if !foreign_key_checks() {
        return Vec::new();
    }
    let mut checks = Vec::new();
    let mut cascades = Vec::new();
    let targets = if delete.tables.is_empty() {
        affected_tables(is, current_db, &delete.table, delete.join.as_ref())
    } else {
        delete
            .tables
            .iter()
            .filter_map(|name| resolve_table(is, current_db, name))
            .collect()
    };
    for table in targets {
        // Every referenced key row disappears, so every child FK is affected.
        let (child_checks, child_cascades) = split_by_action(collect_child_fks(
            is,
            current_db,
            table,
            &[],
            false,
        ));
        checks.extend(child_checks);
        cascades.extend(child_cascades);
    }
    finish(checks, cascades)
}

/// Updates reach children through `ON UPDATE`; a RESTRICT-like FK blocks the
/// statement with `check_not_exist`.
fn collect_child_fks_update(
    is: &InfoSchema,
    current_db: &str,
    parent: &TableInfo,
    cols: &[String],
    checks: &mut Vec<Keyed>,
    cascades: &mut Vec<Keyed>,
) {
    let (child_checks, child_cascades) =
        split_by_action(collect_child_fks(is, current_db, parent, cols, true));
    checks.extend(child_checks);
    cascades.extend(child_cascades);
}

fn split_by_action(nodes: Vec<Keyed>) -> (Vec<Keyed>, Vec<Keyed>) {
    nodes.into_iter().partition(|(_, _, _, node)| {
        !matches!(
            node.action,
            FkAction::OnDeleteCascade | FkAction::OnUpdateCascade
        )
    })
}

/// Children (in every schema) whose FK references `parent` on one of `cols`
/// (empty `cols` = any referenced column): plans a cascade when the action is
/// CASCADE, otherwise a blocking check.
fn collect_child_fks(
    is: &InfoSchema,
    _current_db: &str,
    parent: &TableInfo,
    cols: &[String],
    on_update: bool,
) -> Vec<Keyed> {
    let mut out = Vec::new();
    for schema in is.all_schemas() {
        let db = schema.name.o().to_string();
        for child in is.schema_tables(&db) {
            for (order, fk) in child.foreign_keys.iter().enumerate() {
                if !fk.ref_table.eq_ignore_ascii_case(parent.name.o()) {
                    continue;
                }
                // `ref_cols` empty means the parent PRIMARY KEY is referenced.
                let touched = cols.is_empty() || cols_cover(&fk.ref_cols, cols);
                if !touched {
                    continue;
                }
                let cascade = if on_update {
                    fk.on_update_cascade
                } else {
                    fk.on_delete_cascade
                };
                let action = if cascade {
                    if on_update {
                        FkAction::OnUpdateCascade
                    } else {
                        FkAction::OnDeleteCascade
                    }
                } else {
                    FkAction::CheckNotExist
                };
                let node = FkNode {
                    table: child.name.o().to_string(),
                    index: cover_index(child, &fk.cols),
                    fk_name: fk.name.clone(),
                    action,
                };
                out.push((
                    parent.name.o().to_string(),
                    child.name.o().to_string(),
                    order,
                    node,
                ));
            }
        }
    }
    out
}

fn check_exist_node(
    is: &InfoSchema,
    current_db: &str,
    _child: &TableInfo,
    fk: &ForeignKeyInfo,
) -> Option<FkNode> {
    let parent = is.table_by_name(current_db, &fk.ref_table)?;
    let mut ref_cols = fk.ref_cols.clone();
    if ref_cols.is_empty() {
        ref_cols = parent
            .indices
            .iter()
            .find(|idx| idx.primary)
            .map(|idx| idx.columns.iter().map(|c| c.name.o().to_string()).collect())
            .unwrap_or_default();
    }
    Some(FkNode {
        table: parent.name.o().to_string(),
        index: cover_index(parent, &ref_cols),
        fk_name: fk.name.clone(),
        action: FkAction::CheckExist,
    })
}

fn finish(mut checks: Vec<Keyed>, mut cascades: Vec<Keyed>) -> Vec<FkNode> {
    let by_key = |a: &Keyed, b: &Keyed| {
        (a.0.to_ascii_lowercase(), a.1.to_ascii_lowercase(), a.2).cmp(&(
            b.0.to_ascii_lowercase(),
            b.1.to_ascii_lowercase(),
            b.2,
        ))
    };
    checks.sort_by(by_key);
    cascades.sort_by(by_key);
    checks
        .into_iter()
        .chain(cascades)
        .map(|(_, _, _, node)| node)
        .collect()
}

/// `UPDATE` targets: the statement table plus the other side of a multi-table
/// join (`UPDATE a, b SET …`).
fn affected_tables<'a>(
    is: &'a InfoSchema,
    current_db: &str,
    table: &TableName,
    join: Option<&sealdb_parser::ast::Join>,
) -> Vec<&'a TableInfo> {
    let mut out = Vec::new();
    if let Some(info) = resolve_table(is, current_db, table) {
        out.push(info);
    }
    if let Some(join) = join {
        for name in [&join.left, &join.right] {
            if let Some(info) = resolve_table(is, current_db, name)
                && !out.iter().any(|t| t.id == info.id)
            {
                out.push(info);
            }
        }
    }
    out
}

/// Assignments in `UPDATE` that target `table` (qualified by alias / name, or
/// unqualified on a single-table update).
fn assignments_for<'a>(
    update: &'a UpdateStmt,
    table: &TableInfo,
) -> Vec<&'a sealdb_parser::ast::Assignment> {
    let name = table.name.o().to_ascii_lowercase();
    update
        .list
        .iter()
        .filter(|assign| match &assign.table {
            Some(qualifier) => qualifier.eq_ignore_ascii_case(&name),
            None => true,
        })
        .collect()
}

fn resolve_table<'a>(
    is: &'a InfoSchema,
    current_db: &str,
    name: &TableName,
) -> Option<&'a TableInfo> {
    let db = name.schema.as_deref().unwrap_or(current_db);
    is.table_by_name(db, name.physical.as_deref().unwrap_or(&name.name))
}

/// Index whose leading columns are exactly `cols` (or cover them as a prefix).
/// `None` when the integer row handle serves as the FK index.
fn cover_index(table: &TableInfo, cols: &[String]) -> Option<String> {
    if cols.is_empty() {
        return None;
    }
    let matches = |idx: &sealdb_meta::IndexInfo| {
        idx.columns.len() >= cols.len()
            && idx
                .columns
                .iter()
                .zip(cols.iter())
                .all(|(ic, want)| ic.name.o().eq_ignore_ascii_case(want))
    };
    if table.pk_is_handle
        && let Some(pk) = table.indices.iter().find(|idx| idx.primary)
        && matches(pk)
    {
        return None;
    }
    table
        .indices
        .iter()
        .find(|idx| matches(idx))
        .map(|idx| idx.name.o().to_string())
}

fn cols_cover(need: &[String], have: &[String]) -> bool {
    need.iter()
        .all(|col| have.iter().any(|h| h.eq_ignore_ascii_case(col)))
}
