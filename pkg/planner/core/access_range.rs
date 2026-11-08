//! Index access-range construction — the `util/ranger` subset SealDB needs for
//! a single leading index column.
//!
//! TiDB's detacher pulls every comparison on the index prefix out of the cop
//! Selection and hands the merged bounds to `IndexRangeScan`, which keeps only
//! rows inside `range:…`. SealDB already seeks with the same bounds at
//! execution time, so the planner only has to expose the range to EXPLAIN and
//! to the cardinality estimate.
use super::physical_plans::IndexScanRange;
use sealdb_meta::{IndexInfo, TableInfo};
use sealdb_parser::ast::expressions::{ColumnRange, ExprNode};
use sealdb_statistics::pseudo;

pub struct IndexAccessRange {
    /// `range:…` text + estRows for EXPLAIN.
    pub range: IndexScanRange,
    /// Conjuncts fully encoded by the range (they leave the cop Selection).
    pub consumed: Vec<ExprNode>,
}

/// Build the leading-column access range of `index` from `conjuncts`.
pub fn index_access_range(
    info: &TableInfo,
    index: &IndexInfo,
    conjuncts: &[ExprNode],
    table_rows: f64,
) -> Option<IndexAccessRange> {
    let leading = index.columns.first()?;
    let column = leading.name.o();
    let column_info = info
        .columns
        .iter()
        .find(|c| c.name.o().eq_ignore_ascii_case(&column))?;
    // TIMESTAMP index keys are stored in UTC, so session-local bounds cannot be
    // pushed as a seek; the executor keeps the full scan + Selection instead.
    if column_info.field_type.tp == sealdb_types::TYPE_TIMESTAMP {
        return None;
    }
    let mut bounds: pseudo::Bounds = (None, None);
    let mut consumed = Vec::new();
    for conjunct in conjuncts {
        let Some((name, range)) = pseudo::conjunct_column_range(conjunct) else {
            continue;
        };
        if !name.eq_ignore_ascii_case(&column) {
            continue;
        }
        // Ranger converts each bound to the column charset; a binary literal
        // that is not valid UTF-8 must surface the conversion error from the
        // cop Selection instead of being seeked as raw bytes.
        if !bound_fits_column(&column_info.field_type, &range) {
            return None;
        }
        merge_range(&mut bounds, &range);
        consumed.push(conjunct.clone());
    }
    if bounds.0.is_none() && bounds.1.is_none() {
        return None;
    }
    // `PrefixEqualLen`: the leading column is pinned only by `col = const`
    // bounds, which ranger folds to an inclusive point range.
    let pinned = matches!(
        (&bounds.0, &bounds.1),
        (Some((low, true)), Some((high, true))) if low.compare(high) == std::cmp::Ordering::Equal
    );
    let rows = pseudo::index_range_rows(
        table_rows,
        bounds.0.as_ref(),
        bounds.1.as_ref(),
        index.columns.len(),
        usize::from(pinned),
        index.unique,
    );
    Some(IndexAccessRange {
        range: IndexScanRange {
            text: pseudo::range_text(bounds.0.as_ref(), bounds.1.as_ref()),
            rows,
            filter_rows: pseudo::filter_rows(conjuncts, table_rows),
            pseudo: true,
        },
        consumed,
    })
}

/// Intersect one conjunct's bounds into the accumulated leading-column range.
fn merge_range(target: &mut pseudo::Bounds, range: &ColumnRange) {
    pseudo::merge_bounds(target, &(range.low.clone(), range.high.clone()));
}

/// Can both bounds be converted to `field_type` like TiDB `ranger` does?
fn bound_fits_column(field_type: &sealdb_types::FieldType, range: &ColumnRange) -> bool {
    let fits = |bound: &Option<(sealdb_types::Datum, bool)>| match bound {
        // Binary values: string literals, and hex/bit literals (which arrive as
        // `Datum::Bit`) — both need a charset conversion to the column.
        Some((sealdb_types::Datum::Bytes(bytes) | sealdb_types::Datum::Bit(bytes), _)) => {
            field_type.charset == "binary" || std::str::from_utf8(bytes).is_ok()
        }
        _ => true,
    };
    fits(&range.low) && fits(&range.high)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_meta::{ColumnInfo, IndexColumn};
    use sealdb_types::FieldType;
    use sealdb_parser::Parser;

    fn int_column(name: &str, offset: i32) -> ColumnInfo {
        ColumnInfo::new(
            offset as i64 + 1,
            offset,
            name,
            FieldType::new_longlong(),
        )
    }

    fn table_info(
        columns: Vec<ColumnInfo>,
        index_columns: &[&str],
        unique: bool,
    ) -> (TableInfo, IndexInfo) {
        let mut info = TableInfo::new(1, "t", columns);
        let cols: Vec<IndexColumn> = index_columns
            .iter()
            .enumerate()
            .map(|(offset, name)| IndexColumn::new(*name, offset as i32))
            .collect();
        let mut index = IndexInfo::new(1, "idx", "t", cols);
        index.unique = unique;
        info.indices = vec![index.clone()];
        (info, index)
    }

    fn selection(sql: &str) -> Vec<ExprNode> {
        let stmt = Parser::new()
            .parse_one_stmt(&format!("SELECT * FROM t WHERE {sql}"))
            .expect("parse");
        let sealdb_parser::ast::Statement::Select(select) = stmt else {
            panic!("not a select");
        };
        select
            .selection
            .expect("selection")
            .split_cnf()
    }

    #[test]
    fn test_less_than_becomes_range() {
        let (info, index) =
            table_info(vec![int_column("a", 0), int_column("b", 1)], &["a"], false);
        let access =
            index_access_range(&info, &index, &selection("a < 3"), 10_000.0).expect("range");
        assert_eq!(access.range.text, "[-inf,3)");
        assert!((access.range.rows - (10_000.0 / 3.0 - 10.0)).abs() < 1e-6);
        assert_eq!(access.consumed.len(), 1);
    }

    #[test]
    fn test_composite_prefix_only_consumes_leading_column() {
        let (info, index) = table_info(
            vec![int_column("a", 0), int_column("b", 1)],
            &["a", "b"],
            false,
        );
        let access =
            index_access_range(&info, &index, &selection("a < 3 and b < 3"), 10_000.0)
                .expect("range");
        assert_eq!(access.range.text, "[-inf,3)");
        assert_eq!(access.consumed.len(), 1);
        assert!((access.range.rows - (10_000.0 / 3.0 - 10.0)).abs() < 1e-6);
    }

    #[test]
    fn test_unrelated_column_has_no_range() {
        let (info, index) =
            table_info(vec![int_column("a", 0), int_column("b", 1)], &["a"], false);
        assert!(index_access_range(&info, &index, &selection("b < 3"), 10_000.0).is_none());
    }
}
