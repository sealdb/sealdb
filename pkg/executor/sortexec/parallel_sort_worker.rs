// parallel sort workers (TiDB pkg/executor/sortexec/parallel_sort_worker.go).
// Completeness wave / not this feature: chunk-channel fetcher, parallel spill helper.
use crate::sortexec::sort::compare_rows;
use sealdb_errno::{ERR_UNKNOWN_ERROR, Error, Result};
use sealdb_expression::{collation_mode, with_collation};
use sealdb_parser::ast::ByItem;
use sealdb_types::Datum;
use std::thread;

pub fn sort_concurrency() -> usize {
    std::env::var("SEALDB_SORT_CONCURRENCY")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|n: &usize| *n > 0)
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .clamp(1, 8)
        })
}

pub fn parallel_sort_rows(
    rows: &mut Vec<Vec<Datum>>,
    by: &[ByItem],
    schema: &[String],
) -> Result<()> {
    parallel_sort_rows_with(rows, by, schema, sort_concurrency())
}

pub fn parallel_sort_rows_with(
    rows: &mut Vec<Vec<Datum>>,
    by: &[ByItem],
    schema: &[String],
    concurrency: usize,
) -> Result<()> {
    let workers = concurrency.max(1).min(rows.len().max(1));
    if workers <= 1 || rows.len() <= 1 {
        rows.sort_by(|a, b| compare_rows(a, b, by, schema));
        return Ok(());
    }
    let chunk_len = rows.len().div_ceil(workers);
    let mut parts = Vec::with_capacity(workers);
    while !rows.is_empty() {
        let take = chunk_len.min(rows.len());
        parts.push(rows.drain(..take).collect::<Vec<_>>());
    }
    let mode = collation_mode();
    let mut handles = Vec::with_capacity(parts.len());
    for mut part in parts {
        let by = by.to_vec();
        let schema = schema.to_vec();
        handles.push(thread::spawn(move || {
            with_collation(mode, || {
                part.sort_by(|a, b| compare_rows(a, b, &by, &schema));
                part
            })
        }));
    }
    let mut sorted = Vec::with_capacity(handles.len());
    for handle in handles {
        sorted.push(
            handle
                .join()
                .map_err(|_| Error::new(ERR_UNKNOWN_ERROR, "parallel sort worker panicked"))?,
        );
    }
    *rows = merge_sorted_parts(sorted, by, schema);
    Ok(())
}

fn merge_sorted_parts(
    parts: Vec<Vec<Vec<Datum>>>,
    by: &[ByItem],
    schema: &[String],
) -> Vec<Vec<Datum>> {
    let mut iters: Vec<_> = parts.into_iter().map(|part| part.into_iter()).collect();
    let mut heads: Vec<Option<Vec<Datum>>> = iters.iter_mut().map(|it| it.next()).collect();
    let mut out = Vec::new();
    loop {
        let mut best: Option<usize> = None;
        for (i, head) in heads.iter().enumerate() {
            let Some(row) = head else {
                continue;
            };
            match best {
                None => best = Some(i),
                Some(cur) => {
                    if compare_rows(
                        row,
                        heads[cur]
                            .as_ref()
                            .expect("best index still has a head row"),
                        by,
                        schema,
                    )
                    .is_lt()
                    {
                        best = Some(i);
                    }
                }
            }
        }
        let Some(i) = best else {
            break;
        };
        out.push(heads[i].take().expect("best head row"));
        heads[i] = iters[i].next();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sealdb_parser::ast::expressions::{ColumnNameExpr, ExprNode};

    fn by_id() -> Vec<ByItem> {
        vec![ByItem {
            expr: ExprNode::Column(ColumnNameExpr {
                schema: None,
                table: None,
                name: "id".to_string(),
            }),
            desc: false,
        }]
    }

    #[test]
    fn test_parallel_sort_matches_serial() {
        let schema = vec!["id".to_string(), "name".to_string()];
        let rows = vec![
            vec![Datum::I64(3), Datum::Bytes(b"c".to_vec())],
            vec![Datum::I64(1), Datum::Bytes(b"a".to_vec())],
            vec![Datum::I64(4), Datum::Bytes(b"d".to_vec())],
            vec![Datum::I64(2), Datum::Bytes(b"b".to_vec())],
            vec![Datum::I64(0), Datum::Bytes(b"z".to_vec())],
        ];
        let mut serial = rows.clone();
        parallel_sort_rows_with(&mut serial, &by_id(), &schema, 1).unwrap();
        let mut parallel = rows;
        parallel_sort_rows_with(&mut parallel, &by_id(), &schema, 3).unwrap();
        assert_eq!(serial, parallel);
        assert_eq!(serial[0][0], Datum::I64(0));
        assert_eq!(serial[4][0], Datum::I64(4));
    }
}
