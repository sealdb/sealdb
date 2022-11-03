use async_trait::async_trait;
use common::Result;

/// KV 存储抽象接口
#[async_trait]
pub trait KVStore: Send + Sync {
    /// 获取值
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// 设置值
    async fn put(&self, key: &[u8], value: &[u8]) -> Result<()>;

    /// 删除值
    async fn delete(&self, key: &[u8]) -> Result<()>;

    /// 扫描键值对
    async fn scan(
        &self,
        start_key: &[u8],
        end_key: &[u8],
        limit: u32,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;

    /// 开始事务
    async fn begin_transaction(&self) -> Result<Box<dyn KVTransaction>>;
}

/// KV 事务抽象接口
#[async_trait]
pub trait KVTransaction: Send + Sync {
    /// 事务内获取值
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    /// 事务内设置值
    async fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()>;

    /// 事务内删除值
    async fn delete(&mut self, key: &[u8]) -> Result<()>;

    /// 事务内扫描键值对
    async fn scan(
        &mut self,
        start_key: &[u8],
        end_key: &[u8],
        limit: u32,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;

    /// 提交事务
    async fn commit(self: Box<Self>) -> Result<()>;

    /// 回滚事务
    async fn rollback(self: Box<Self>) -> Result<()>;
}

/// TiKV 存储实现
pub struct TiKVStore {
    client: crate::TiKVClient,
}

impl TiKVStore {
    pub fn new(client: crate::TiKVClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl KVStore for TiKVStore {
    async fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.client.get(key).await
    }

    async fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.client.put(key, value).await
    }

    async fn delete(&self, key: &[u8]) -> Result<()> {
        self.client.delete(key).await
    }

    async fn scan(
        &self,
        start_key: &[u8],
        end_key: &[u8],
        limit: u32,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.client.scan(start_key, end_key, limit).await
    }

    async fn begin_transaction(&self) -> Result<Box<dyn KVTransaction>> {
        let txn = self.client.begin_transaction().await?;
        Ok(Box::new(TiKVTransactionWrapper::new(txn)))
    }
}

/// TiKV 事务包装器
pub struct TiKVTransactionWrapper {
    transaction: crate::TiKVTransaction,
}

impl TiKVTransactionWrapper {
    pub fn new(transaction: crate::TiKVTransaction) -> Self {
        Self { transaction }
    }
}

#[async_trait]
impl KVTransaction for TiKVTransactionWrapper {
    async fn get(&mut self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.transaction.get(key).await
    }

    async fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.transaction.put(key, value).await
    }

    async fn delete(&mut self, key: &[u8]) -> Result<()> {
        self.transaction.delete(key).await
    }

    async fn scan(
        &mut self,
        start_key: &[u8],
        end_key: &[u8],
        limit: u32,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.transaction.scan(start_key, end_key, limit).await
    }

    async fn commit(self: Box<Self>) -> Result<()> {
        self.transaction.commit().await
    }

    async fn rollback(self: Box<Self>) -> Result<()> {
        self.transaction.rollback().await
    }
}
