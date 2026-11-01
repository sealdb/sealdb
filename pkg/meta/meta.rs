use crate::model::{DBInfo, TableInfo};
use sealdb_errno::{
    ERR_BAD_DB, ERR_DB_CREATE_EXISTS, ERR_DB_DROP_EXISTS, ERR_NO_SUCH_TABLE, ERR_TABLE_EXISTS,
    Error, Result,
};
use sealdb_kv::Key;
use sealdb_util::codec::{encode_bytes, encode_uint};
use std::collections::BTreeMap;

pub const META_PREFIX: &[u8] = b"m";
pub const NEXT_GLOBAL_ID_KEY: &[u8] = b"NextGlobalID";
pub const SCHEMA_VERSION_KEY: &[u8] = b"SchemaVersionKey";
pub const DBS: &[u8] = b"DBs";
pub const DB_PREFIX: &str = "DB";
pub const TABLE_PREFIX: &str = "Table";
pub const HASH_DATA: u8 = b'h';
pub const STRING_DATA: u8 = b's';

pub fn db_key(db_id: i64) -> Vec<u8> {
    format!("{DB_PREFIX}:{db_id}").into_bytes()
}

pub fn table_key(table_id: i64) -> Vec<u8> {
    format!("{TABLE_PREFIX}:{table_id}").into_bytes()
}

pub fn is_db_key(key: &[u8]) -> bool {
    key.starts_with(format!("{DB_PREFIX}:").as_bytes())
}

pub fn is_table_key(key: &[u8]) -> bool {
    key.starts_with(format!("{TABLE_PREFIX}:").as_bytes())
}

pub fn parse_db_key(key: &[u8]) -> Result<i64> {
    parse_prefixed_id(key, DB_PREFIX, "fail to parse dbKey")
}

pub fn parse_table_key(key: &[u8]) -> Result<i64> {
    parse_prefixed_id(key, TABLE_PREFIX, "fail to parse tableKey")
}

fn parse_prefixed_id(key: &[u8], prefix: &str, err: &str) -> Result<i64> {
    let text =
        std::str::from_utf8(key).map_err(|_| Error::new(sealdb_errno::ERR_INVALID_KEY, err))?;
    let rest = text
        .strip_prefix(&format!("{prefix}:"))
        .ok_or_else(|| Error::new(sealdb_errno::ERR_INVALID_KEY, err))?;
    rest.parse::<i64>()
        .map_err(|_| Error::new(sealdb_errno::ERR_INVALID_KEY, err))
}

pub fn encode_string_data_key(key: &[u8]) -> Key {
    let mut buf = Vec::with_capacity(META_PREFIX.len() + key.len() + 24);
    buf.extend_from_slice(META_PREFIX);
    encode_bytes(&mut buf, key);
    encode_uint(&mut buf, u64::from(STRING_DATA));
    Key::from_raw(buf)
}

pub fn encode_hash_data_key(key: &[u8], field: &[u8]) -> Key {
    let mut buf = Vec::with_capacity(META_PREFIX.len() + key.len() + field.len() + 32);
    buf.extend_from_slice(META_PREFIX);
    encode_bytes(&mut buf, key);
    encode_uint(&mut buf, u64::from(HASH_DATA));
    encode_bytes(&mut buf, field);
    Key::from_raw(buf)
}

pub fn encode_hash_data_key_prefix(key: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(META_PREFIX.len() + key.len() + 24);
    buf.extend_from_slice(META_PREFIX);
    encode_bytes(&mut buf, key);
    encode_uint(&mut buf, u64::from(HASH_DATA));
    buf
}

#[derive(Clone, Debug, Default)]
pub struct Mutator {
    kv: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Mutator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn gen_global_id(&mut self) -> Result<i64> {
        self.inc_int(NEXT_GLOBAL_ID_KEY, 1)
    }

    pub fn get_global_id(&self) -> Result<i64> {
        self.get_int(NEXT_GLOBAL_ID_KEY)
    }

    pub fn gen_schema_version(&mut self) -> Result<i64> {
        self.inc_int(SCHEMA_VERSION_KEY, 1)
    }

    pub fn get_schema_version(&self) -> Result<i64> {
        self.get_int(SCHEMA_VERSION_KEY)
    }

    pub fn create_database(&mut self, db: &DBInfo) -> Result<()> {
        let field = db_key(db.id);
        if self.hget(DBS, &field)?.is_some() {
            return Err(Error::new(
                ERR_DB_CREATE_EXISTS,
                format!("Can't create database '{}'; database exists", db.name.o()),
            ));
        }
        self.hset(DBS, &field, &serde_json::to_vec(db).map_err(json_err)?)
    }

    pub fn drop_database(&mut self, db_id: i64) -> Result<()> {
        let field = db_key(db_id);
        if self.hget(DBS, &field)?.is_none() {
            return Err(Error::new(
                ERR_DB_DROP_EXISTS,
                format!("Can't drop database '{db_id}'; database doesn't exist"),
            ));
        }
        self.hdel(DBS, &field);
        self.hclear(&field);
        Ok(())
    }

    pub fn create_table_or_view(&mut self, db_id: i64, table: &TableInfo) -> Result<()> {
        let db_field = db_key(db_id);
        if self.hget(DBS, &db_field)?.is_none() {
            return Err(Error::new(
                ERR_BAD_DB,
                format!("Unknown database '{db_id}'"),
            ));
        }
        let table_field = table_key(table.id);
        if self.hget(&db_field, &table_field)?.is_some() {
            return Err(Error::new(
                ERR_TABLE_EXISTS,
                format!("Table '{}' already exists", table.name.o()),
            ));
        }
        self.hset(
            &db_field,
            &table_field,
            &serde_json::to_vec(table).map_err(json_err)?,
        )
    }

    pub fn drop_table_or_view(&mut self, db_id: i64, table_id: i64) -> Result<()> {
        let db_field = db_key(db_id);
        let table_field = table_key(table_id);
        if self.hget(&db_field, &table_field)?.is_none() {
            return Err(Error::new(
                ERR_NO_SUCH_TABLE,
                format!("Table '{db_id}.{table_id}' doesn't exist"),
            ));
        }
        self.hdel(&db_field, &table_field);
        Ok(())
    }

    pub fn list_databases(&self) -> Result<Vec<DBInfo>> {
        self.hget_all(DBS)?
            .into_iter()
            .map(|(_, value)| serde_json::from_slice(&value).map_err(json_err))
            .collect()
    }

    pub fn list_tables(&self, db_id: i64) -> Result<Vec<TableInfo>> {
        self.hget_all(&db_key(db_id))?
            .into_iter()
            .filter(|(field, _)| is_table_key(field))
            .map(|(_, value)| serde_json::from_slice(&value).map_err(json_err))
            .collect()
    }

    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.kv.get(key).map(Vec::as_slice)
    }

    pub fn encoded_pairs(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.kv.iter().map(|(k, v)| (k.as_slice(), v.as_slice()))
    }

    fn inc_int(&mut self, key: &[u8], step: i64) -> Result<i64> {
        let next = self.get_int(key)? + step;
        let encoded = encode_string_data_key(key);
        self.kv
            .insert(encoded.into_bytes(), next.to_string().into_bytes());
        Ok(next)
    }

    fn get_int(&self, key: &[u8]) -> Result<i64> {
        match self.kv.get(encode_string_data_key(key).as_bytes()) {
            Some(raw) => {
                let text = std::str::from_utf8(raw).map_err(|_| {
                    Error::new(sealdb_errno::ERR_INVALID_KEY, "invalid integer meta value")
                })?;
                text.parse::<i64>().map_err(|_| {
                    Error::new(sealdb_errno::ERR_INVALID_KEY, "invalid integer meta value")
                })
            }
            None => Ok(0),
        }
    }

    fn hset(&mut self, key: &[u8], field: &[u8], value: &[u8]) -> Result<()> {
        self.kv.insert(
            encode_hash_data_key(key, field).into_bytes(),
            value.to_vec(),
        );
        Ok(())
    }

    fn hget(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .kv
            .get(encode_hash_data_key(key, field).as_bytes())
            .cloned())
    }

    fn hdel(&mut self, key: &[u8], field: &[u8]) {
        self.kv.remove(encode_hash_data_key(key, field).as_bytes());
    }

    fn hclear(&mut self, key: &[u8]) {
        let prefix = encode_hash_data_key_prefix(key);
        self.kv.retain(|k, _| !k.starts_with(&prefix));
    }

    fn hget_all(&self, key: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let prefix = encode_hash_data_key_prefix(key);
        let mut out = Vec::new();
        for (encoded, value) in &self.kv {
            if encoded.starts_with(&prefix) {
                let field = &encoded[prefix.len()..];
                let (_, decoded) = sealdb_util::codec::decode_bytes(field, false)?;
                out.push((decoded, value.clone()));
            }
        }
        Ok(out)
    }
}

fn json_err(err: serde_json::Error) -> Error {
    Error::new(sealdb_errno::ERR_UNKNOWN_ERROR, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnInfo, TableInfo};
    use sealdb_types::FieldType;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn test_db_and_table_key() {
        assert_eq!(db_key(1), b"DB:1");
        assert_eq!(table_key(2), b"Table:2");
        assert!(is_db_key(&db_key(1)));
        assert!(is_table_key(&table_key(2)));
        assert_eq!(parse_db_key(&db_key(11)).unwrap(), 11);
        assert_eq!(parse_table_key(&table_key(8)).unwrap(), 8);
    }

    #[test]
    fn test_hash_data_key_layout() {
        let key = encode_hash_data_key(DBS, &db_key(1));
        let mut expected = Vec::new();
        expected.extend_from_slice(b"m");
        encode_bytes(&mut expected, b"DBs");
        encode_uint(&mut expected, u64::from(b'h'));
        encode_bytes(&mut expected, b"DB:1");
        assert_eq!(key.as_bytes(), expected.as_slice());
        assert_eq!(
            key.as_bytes(),
            hex("6d4442730000000000fa000000000000006844423a3100000000fb")
        );
    }

    #[test]
    fn test_create_and_drop_roundtrip() {
        let mut meta = Mutator::new();
        let db_id = meta.gen_global_id().unwrap();
        let db = DBInfo::new(db_id, "db1");
        meta.create_database(&db).unwrap();
        let table_id = meta.gen_global_id().unwrap();
        let table = TableInfo::new(
            table_id,
            "t",
            vec![ColumnInfo::new(1, 0, "id", FieldType::new_longlong())],
        );
        meta.create_table_or_view(db_id, &table).unwrap();
        assert_eq!(meta.list_databases().unwrap().len(), 1);
        assert_eq!(meta.list_tables(db_id).unwrap()[0].name.o(), "t");
        let db_kv = encode_hash_data_key(DBS, &db_key(db_id));
        assert!(meta.get(db_kv.as_bytes()).is_some());
        meta.drop_table_or_view(db_id, table_id).unwrap();
        meta.drop_database(db_id).unwrap();
        assert!(meta.list_databases().unwrap().is_empty());
    }
}
