use sealdb_errno::Result;
use sealdb_kv::Transaction;
use sealdb_meta::{IndexInfo, TableInfo};
use sealdb_tablecodec::{encode_index_key, encode_row_key_with_handle};
use sealdb_types::Datum;

pub(crate) fn index_values(index: &IndexInfo, values: &[Datum]) -> Vec<Datum> {
    index
        .columns
        .iter()
        .map(|col| {
            values
                .get(col.offset as usize)
                .cloned()
                .unwrap_or(Datum::Null)
        })
        .collect()
}

pub(crate) fn write_index_keys(
    txn: &mut dyn Transaction,
    info: &TableInfo,
    handle: i64,
    values: &[Datum],
) -> Result<()> {
    for index in &info.indices {
        let key = encode_index_key(info.id, index.id, &index_values(index, values), handle);
        txn.set(
            key.as_bytes(),
            encode_row_key_with_handle(info.id, handle).as_bytes(),
        )?;
    }
    Ok(())
}

pub(crate) fn delete_index_keys(
    txn: &mut dyn Transaction,
    info: &TableInfo,
    handle: i64,
    values: &[Datum],
) -> Result<()> {
    for index in &info.indices {
        let key = encode_index_key(info.id, index.id, &index_values(index, values), handle);
        txn.delete(key.as_bytes())?;
    }
    Ok(())
}
