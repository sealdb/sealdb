use common::Result;
use async_trait::async_trait;
use crate::executor::execution_models::QueryResult;

/// 基础操作符 trait
#[async_trait]
pub trait Operator {
    async fn execute(&self) -> Result<QueryResult>;
} 