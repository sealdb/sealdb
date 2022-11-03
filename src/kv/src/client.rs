use crate::TiKVTransaction;
use common::{Error, Result};
use tikv_client::{Key, RawClient, Value as TiKVValue};
use tracing::{debug, error, info};

/// TiKV 客户端封装
pub struct TiKVClient {
    client: RawClient,
}

impl TiKVClient {
    pub async fn new(pd_endpoints: Vec<String>) -> Result<Self> {
        let client = RawClient::new(pd_endpoints)
            .await
            .map_err(|e| Error::Storage(format!("Failed to create TiKV client: {e}")))?;

        info!("TiKV client initialized successfully");
        Ok(Self { client })
    }

    pub async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let key = Key::from(key.to_vec());
        let key_clone = key.clone();
        match self.client.get(key).await {
            Ok(Some(value)) => {
                debug!("Get key success: {:?}", key_clone);
                Ok(Some(value))
            }
            Ok(None) => {
                debug!("Key not found: {:?}", key_clone);
                Ok(None)
            }
            Err(e) => {
                error!("Failed to get key {:?}: {}", key_clone, e);
                Err(Error::Storage(format!("Get operation failed: {e}")))
            }
        }
    }

    pub async fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let key = Key::from(key.to_vec());
        let key_clone = key.clone();
        let value = TiKVValue::from(value.to_vec());

        match self.client.put(key, value).await {
            Ok(_) => {
                debug!("Put key success: {:?}", key_clone);
                Ok(())
            }
            Err(e) => {
                error!("Failed to put key {:?}: {}", key_clone, e);
                Err(Error::Storage(format!("Put operation failed: {e}")))
            }
        }
    }

    pub async fn delete(&self, key: &[u8]) -> Result<()> {
        let key = Key::from(key.to_vec());
        let key_clone = key.clone();

        match self.client.delete(key).await {
            Ok(_) => {
                debug!("Delete key success: {:?}", key_clone);
                Ok(())
            }
            Err(e) => {
                error!("Failed to delete key {:?}: {}", key_clone, e);
                Err(Error::Storage(format!("Delete operation failed: {e}")))
            }
        }
    }

    pub async fn scan(
        &self,
        start_key: &[u8],
        end_key: &[u8],
        limit: u32,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let start_key = Key::from(start_key.to_vec());
        let end_key = Key::from(end_key.to_vec());

        match self.client.scan(start_key..end_key, limit).await {
            Ok(pairs) => {
                let result: Vec<(Vec<u8>, Vec<u8>)> = pairs
                    .into_iter()
                    .map(|pair| (pair.key().clone().into(), pair.value().clone()))
                    .collect();
                debug!("Scan success, found {} pairs", result.len());
                Ok(result)
            }
            Err(e) => {
                error!("Failed to scan keys: {}", e);
                Err(Error::Storage(format!("Scan operation failed: {e}")))
            }
        }
    }

    pub async fn begin_transaction(&self) -> Result<TiKVTransaction> {
        // 简化实现，暂时返回空事务
        Ok(TiKVTransaction::new())
    }
}
