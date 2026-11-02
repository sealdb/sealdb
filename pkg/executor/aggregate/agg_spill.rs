// HashAgg leftover: partition input rows by group key and restore per partition.
// Completeness wave: recursive multi-pass spill, parallel workers, partial-agg serialize.
use sealdb_errno::Result;
use sealdb_types::Datum;
use sealdb_util::chunk::{DataInDiskByRows, SPILL_PARTITION_NUM, partition_index};

pub fn new_partitions() -> Result<Vec<DataInDiskByRows>> {
    (0..SPILL_PARTITION_NUM)
        .map(|_| DataInDiskByRows::new())
        .collect()
}

pub fn add_to_partition(
    files: &mut [DataInDiskByRows],
    keys: &[Datum],
    row: &[Datum],
) -> Result<()> {
    let idx = partition_index(keys, files.len());
    files[idx].add_row(row)
}
