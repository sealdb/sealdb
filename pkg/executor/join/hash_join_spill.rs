// HashJoin leftover: partition both sides by join key, then probe per partition.
// Completeness wave: parallel workers, recursive spill, build-side bitmap.
use sealdb_errno::Result;
use sealdb_expression::eval_expr_with_row;
use sealdb_parser::ast::expressions::ExprNode;
use sealdb_types::Datum;
use sealdb_util::chunk::{DataInDiskByRows, SPILL_PARTITION_NUM, partition_index};

pub fn partition_rows(
    rows: Vec<Vec<Datum>>,
    schema: &[String],
    key: &ExprNode,
    n: usize,
) -> Result<Vec<DataInDiskByRows>> {
    let n = n.max(1);
    let mut files: Vec<DataInDiskByRows> = (0..n)
        .map(|_| DataInDiskByRows::new())
        .collect::<Result<_>>()?;
    for row in rows {
        let value = eval_expr_with_row(key, schema, &row)?;
        let idx = partition_index(std::slice::from_ref(&value), files.len());
        files[idx].add_row(&row)?;
    }
    for file in &mut files {
        file.finish_write()?;
    }
    Ok(files)
}

pub fn partition_count() -> usize {
    SPILL_PARTITION_NUM
}
