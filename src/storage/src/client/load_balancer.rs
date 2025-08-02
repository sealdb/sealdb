//! 负载均衡器实现
//!
//! 提供多种负载均衡策略

use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::RwLock;
use tracing::{debug, info};

use crate::common::*;

/// 负载均衡策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadBalancingStrategy {
    RoundRobin,
    LeastConnections,
    Random,
    Weighted,
}

/// 负载均衡器
pub struct LoadBalancer {
    strategy: LoadBalancingStrategy,
    connections: Arc<RwLock<HashMap<EngineType, Vec<ConnectionInfo>>>>,
    round_robin_index: Arc<RwLock<HashMap<EngineType, usize>>>,
}

/// 连接信息
#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub id: String,
    pub engine_type: EngineType,
    pub active_connections: u32,
    pub total_connections: u32,
    pub last_used: std::time::Instant,
    pub weight: f64,
}

impl LoadBalancer {
    /// 创建新的负载均衡器
    pub fn new() -> Self {
        Self {
            strategy: LoadBalancingStrategy::RoundRobin,
            connections: Arc::new(RwLock::new(HashMap::new())),
            round_robin_index: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 设置负载均衡策略
    pub fn set_strategy(&mut self, strategy: LoadBalancingStrategy) {
        self.strategy = strategy;
        info!("Load balancer strategy set to: {:?}", strategy);
    }

    /// 添加连接
    pub fn add_connection(&self, engine_type: EngineType, connection_info: ConnectionInfo) {
        let mut connections = self.connections.write();
        connections.entry(engine_type).or_insert_with(Vec::new).push(connection_info);
        debug!("Added connection for engine: {:?}", engine_type);
    }

    /// 移除连接
    pub fn remove_connection(&self, engine_type: EngineType, connection_id: &str) {
        let mut connections = self.connections.write();
        if let Some(engine_connections) = connections.get_mut(&engine_type) {
            engine_connections.retain(|conn| conn.id != connection_id);
            debug!("Removed connection {} for engine: {:?}", connection_id, engine_type);
        }
    }

    /// 选择连接
    pub fn select_connection(&self, engine_type: EngineType) -> Option<ConnectionInfo> {
        let connections = self.connections.read();
        let engine_connections = connections.get(&engine_type)?;

        if engine_connections.is_empty() {
            return None;
        }

        let selected = match self.strategy {
            LoadBalancingStrategy::RoundRobin => self.select_round_robin(engine_type, engine_connections),
            LoadBalancingStrategy::LeastConnections => self.select_least_connections(engine_connections),
            LoadBalancingStrategy::Random => self.select_random(engine_connections),
            LoadBalancingStrategy::Weighted => self.select_weighted(engine_connections),
        };

        selected
    }

    /// 轮询选择
    fn select_round_robin(&self, engine_type: EngineType, connections: &[ConnectionInfo]) -> Option<ConnectionInfo> {
        let mut index = self.round_robin_index.write();
        let current_index = index.entry(engine_type).or_insert(0);
        let selected = connections.get(*current_index).cloned();

        if let Some(_) = selected {
            *current_index = (*current_index + 1) % connections.len();
        }

        selected
    }

    /// 最少连接选择
    fn select_least_connections(&self, connections: &[ConnectionInfo]) -> Option<ConnectionInfo> {
        connections.iter()
            .min_by_key(|conn| conn.active_connections)
            .cloned()
    }

    /// 随机选择
    fn select_random(&self, connections: &[ConnectionInfo]) -> Option<ConnectionInfo> {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        std::time::Instant::now().hash(&mut hasher);
        let hash = hasher.finish();
        let index = (hash as usize) % connections.len();

        connections.get(index).cloned()
    }

    /// 加权选择
    fn select_weighted(&self, connections: &[ConnectionInfo]) -> Option<ConnectionInfo> {
        let total_weight: f64 = connections.iter().map(|conn| conn.weight).sum();
        if total_weight == 0.0 {
            return connections.first().cloned();
        }

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        std::time::Instant::now().hash(&mut hasher);
        let random = (hasher.finish() as f64) / (u64::MAX as f64);

        let mut cumulative_weight = 0.0;
        for connection in connections {
            cumulative_weight += connection.weight;
            if random <= cumulative_weight / total_weight {
                return Some(connection.clone());
            }
        }

        connections.last().cloned()
    }

    /// 更新连接统计信息
    pub fn update_connection_stats(&self, engine_type: EngineType, connection_id: &str, active_connections: u32) {
        let mut connections = self.connections.write();
        if let Some(engine_connections) = connections.get_mut(&engine_type) {
            for conn in engine_connections {
                if conn.id == connection_id {
                    conn.active_connections = active_connections;
                    conn.last_used = std::time::Instant::now();
                    break;
                }
            }
        }
    }

    /// 获取连接统计信息
    pub fn get_connection_stats(&self, engine_type: EngineType) -> Vec<ConnectionInfo> {
        let connections = self.connections.read();
        connections.get(&engine_type).cloned().unwrap_or_default()
    }
}

impl Default for LoadBalancer {
    fn default() -> Self {
        Self::new()
    }
}