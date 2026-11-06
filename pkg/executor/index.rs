use sealdb_errno::{ERR_DUP_ENTRY, Error, Result};
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
    physical_id: i64,
) -> Result<()> {
    for index in &info.indices {
        let key = encode_index_key(
            physical_id,
            index.id,
            &index_values(index, values),
            handle,
            index.unique,
        );
        if index.unique && txn.get(key.as_bytes())?.is_some() {
            let value = index_values(index, values)
                .iter()
                .map(|v| v.display())
                .collect::<Vec<_>>()
                .join("-");
            let name = if index.primary {
                "PRIMARY".to_string()
            } else {
                index.name.o().to_string()
            };
            return Err(Error::new(
                ERR_DUP_ENTRY,
                format!(
                    "Duplicate entry '{value}' for key '{}.{name}'",
                    info.name.o()
                ),
            ));
        }
        txn.set(
            key.as_bytes(),
            encode_row_key_with_handle(physical_id, handle).as_bytes(),
        )?;
    }
    Ok(())
}

pub(crate) fn delete_index_keys(
    txn: &mut dyn Transaction,
    info: &TableInfo,
    handle: i64,
    values: &[Datum],
    physical_id: i64,
) -> Result<()> {
    for index in &info.indices {
        let key = encode_index_key(
            physical_id,
            index.id,
            &index_values(index, values),
            handle,
            index.unique,
        );
        txn.delete(key.as_bytes())?;
    }
    Ok(())
}
