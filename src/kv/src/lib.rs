pub mod client;
pub mod kv_api;
pub mod transaction;

pub use client::TiKVClient;
pub use kv_api::{KVStore, KVTransaction};
pub use transaction::TiKVTransaction;

use common::Result;

/// TiKV 存储引擎
pub struct TiKVEngine {
    client: TiKVClient,
}

impl TiKVEngine {
    pub async fn new(pd_endpoints: Vec<String>) -> Result<Self> {
        let client = TiKVClient::new(pd_endpoints).await?;
        Ok(Self { client })
    }

    pub async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.client.get(key).await
    }

    pub async fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.client.put(key, value).await
    }

    pub async fn delete(&self, key: &[u8]) -> Result<()> {
        self.client.delete(key).await
    }

    pub async fn scan(
        &self,
        start_key: &[u8],
        end_key: &[u8],
        limit: u32,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.client.scan(start_key, end_key, limit).await
    }

    pub async fn begin_transaction(&self) -> Result<TiKVTransaction> {
        self.client.begin_transaction().await
    }
}
