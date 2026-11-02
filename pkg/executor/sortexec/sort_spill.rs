// Sort leftover: sorted runs on disk plus k-way merge.
// Completeness wave: parallel sort, pipelined chunked merge.
use sealdb_errno::Result;
use sealdb_parser::ast::ByItem;
use sealdb_types::Datum;
use sealdb_util::chunk::DataInDiskByRows;

use super::sort::compare_rows;

pub fn spill_sorted_run(
    rows: &mut Vec<Vec<Datum>>,
    by: &[ByItem],
    schema: &[String],
) -> Result<DataInDiskByRows> {
    rows.sort_by(|a, b| compare_rows(a, b, by, schema));
    let mut file = DataInDiskByRows::new()?;
    for row in rows.drain(..) {
        file.add_row(&row)?;
    }
    file.finish_write()?;
    Ok(file)
}

pub fn merge_runs(
    runs: &mut [DataInDiskByRows],
    by: &[ByItem],
    schema: &[String],
) -> Result<Vec<Vec<Datum>>> {
    merge_runs_n(runs, by, schema, usize::MAX)
}

pub fn merge_runs_n(
    runs: &mut [DataInDiskByRows],
    by: &[ByItem],
    schema: &[String],
    max_rows: usize,
) -> Result<Vec<Vec<Datum>>> {
    if max_rows == 0 {
        return Ok(Vec::new());
    }
    let mut heads: Vec<Option<Vec<Datum>>> = Vec::with_capacity(runs.len());
    for run in runs.iter_mut() {
        heads.push(run.next_row()?);
    }
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
        if out.len() >= max_rows {
            break;
        }
        heads[i] = runs[i].next_row()?;
    }
    Ok(out)
}
