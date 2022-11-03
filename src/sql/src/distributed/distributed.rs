use common::Result;
use tracing::{debug, info};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};

// ============================================================================
// 核心数据结构
// ============================================================================

/// 分布式查询计划
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistributedQueryPlan {
    /// 查询片段列表
    pub fragments: Vec<QueryFragment>,
    /// 执行策略
    pub execution_strategy: ExecutionStrategy,
    /// 预估成本
    pub estimated_cost: f64,
    /// 预估行数
    pub estimated_rows: u64,
    /// 分片信息
    pub shard_info: ShardInfo,
}

/// 查询片段
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryFragment {
    /// 片段ID
    pub id: String,
    /// 执行类型
    pub execution_type: ExecutionType,
    /// 分片ID
    pub shard_id: Option<String>,
    /// SQL语句
    pub sql: String,
    /// 依赖的片段
    pub dependencies: Vec<String>,
    /// 目标节点
    pub target_node: Option<String>,
    /// 预估成本
    pub estimated_cost: f64,
    /// 预估行数
    pub estimated_rows: u64,
}

/// 执行类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExecutionType {
    /// 本地执行
    Local,
    /// 分布式执行
    Distributed,
    /// 跨节点连接
    CrossNodeJoin,
    /// 聚合执行
    Aggregate,
}

/// 执行策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionStrategy {
    /// 本地片段
    pub local_fragments: Vec<QueryFragment>,
    /// 分布式片段
    pub distributed_fragments: Vec<QueryFragment>,
    /// 跨节点连接片段
    pub cross_node_joins: Vec<QueryFragment>,
    /// 聚合片段
    pub aggregate_fragments: Vec<QueryFragment>,
}

impl ExecutionStrategy {
    pub fn new() -> Self {
        Self {
            local_fragments: Vec::new(),
            distributed_fragments: Vec::new(),
            cross_node_joins: Vec::new(),
            aggregate_fragments: Vec::new(),
        }
    }

    /// 添加片段到相应类别
    pub fn add_fragment(&mut self, fragment: QueryFragment) {
        match fragment.execution_type {
            ExecutionType::Local => self.local_fragments.push(fragment),
            ExecutionType::Distributed => self.distributed_fragments.push(fragment),
            ExecutionType::CrossNodeJoin => self.cross_node_joins.push(fragment),
            ExecutionType::Aggregate => self.aggregate_fragments.push(fragment),
        }
    }

    /// 获取所有片段
    pub fn get_all_fragments(&self) -> Vec<&QueryFragment> {
        let mut all_fragments = Vec::new();
        all_fragments.extend(self.local_fragments.iter());
        all_fragments.extend(self.distributed_fragments.iter());
        all_fragments.extend(self.cross_node_joins.iter());
        all_fragments.extend(self.aggregate_fragments.iter());
        all_fragments
    }
}

/// 分片信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardInfo {
    /// 分片映射
    pub shard_mapping: HashMap<String, Vec<String>>, // table_name -> shard_ids
    /// 分片分布
    pub shard_distribution: HashMap<String, String>, // shard_id -> node_id
}

impl ShardInfo {
    pub fn new() -> Self {
        Self {
            shard_mapping: HashMap::new(),
            shard_distribution: HashMap::new(),
        }
    }

    /// 添加分片映射
    pub fn add_shard_mapping(&mut self, table_name: String, shard_ids: Vec<String>) {
        self.shard_mapping.insert(table_name, shard_ids);
    }

    /// 添加分片分布
    pub fn add_shard_distribution(&mut self, shard_id: String, node_id: String) {
        self.shard_distribution.insert(shard_id, node_id);
    }

    /// 获取表的分片列表
    pub fn get_table_shards(&self, table_name: &str) -> Option<&Vec<String>> {
        self.shard_mapping.get(table_name)
    }

    /// 获取分片所在的节点
    pub fn get_shard_node(&self, shard_id: &str) -> Option<&String> {
        self.shard_distribution.get(shard_id)
    }
}

// ============================================================================
// 节点和分片管理
// ============================================================================

/// 节点
#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub address: String,
    pub port: u16,
    pub capacity: NodeCapacity,
    pub status: NodeStatus,
}

/// 节点容量
#[derive(Debug, Clone)]
pub struct NodeCapacity {
    pub cpu_cores: u32,
    pub memory_gb: u32,
    pub disk_gb: u32,
}

/// 节点状态
#[derive(Debug, Clone)]
pub enum NodeStatus {
    Online,
    Offline,
    Maintenance,
}

/// 分片
#[derive(Debug, Clone)]
pub struct Shard {
    pub id: String,
    pub table_name: String,
    pub key_range: String,
    pub node_id: String,
    pub row_count: u64,
}

/// 节点管理器
pub struct NodeManager {
    nodes: Mutex<HashMap<String, Node>>,
}

impl NodeManager {
    pub fn new() -> Self {
        let mut nodes = HashMap::new();

        // 添加一些模拟节点
        nodes.insert("node-1".to_string(), Node {
            id: "node-1".to_string(),
            address: "192.168.1.10".to_string(),
            port: 4000,
            capacity: NodeCapacity {
                cpu_cores: 8,
                memory_gb: 16,
                disk_gb: 1000,
            },
            status: NodeStatus::Online,
        });

        nodes.insert("node-2".to_string(), Node {
            id: "node-2".to_string(),
            address: "192.168.1.11".to_string(),
            port: 4000,
            capacity: NodeCapacity {
                cpu_cores: 8,
                memory_gb: 16,
                disk_gb: 1000,
            },
            status: NodeStatus::Online,
        });

        Self {
            nodes: Mutex::new(nodes),
        }
    }

    pub async fn get_available_nodes(&self) -> Result<Vec<Node>> {
        let nodes = self.nodes.lock().unwrap();
        let available_nodes: Vec<Node> = nodes
            .values()
            .filter(|node| matches!(node.status, NodeStatus::Online))
            .cloned()
            .collect();
        Ok(available_nodes)
    }

    pub async fn get_node(&self, node_id: &str) -> Result<Node> {
        let nodes = self.nodes.lock().unwrap();
        nodes
            .get(node_id)
            .cloned()
            .ok_or_else(|| common::Error::Storage(format!("Node {} not found", node_id)))
    }

    pub async fn get_node_load(&self, _node_id: &str) -> Result<f64> {
        // 模拟获取节点负载
        Ok(0.5) // 50% 负载
    }
}

/// 分片管理器
pub struct ShardManager {
    shards: Mutex<HashMap<String, Vec<Shard>>>,
}

impl ShardManager {
    pub fn new() -> Self {
        let mut shards = HashMap::new();

        // 模拟users表的分片
        let mut user_shards = Vec::new();
        for i in 0..4 {
            user_shards.push(Shard {
                id: format!("shard-{}", i),
                table_name: "users".to_string(),
                key_range: format!("{}..{}", i * 1000, (i + 1) * 1000),
                node_id: format!("node-{}", (i % 2) + 1),
                row_count: 1000,
            });
        }
        shards.insert("users".to_string(), user_shards);

        // 模拟orders表的分片
        let mut order_shards = Vec::new();
        for i in 0..4 {
            order_shards.push(Shard {
                id: format!("shard-{}", i),
                table_name: "orders".to_string(),
                key_range: format!("{}..{}", i * 1000, (i + 1) * 1000),
                node_id: format!("node-{}", (i % 2) + 1),
                row_count: 2000,
            });
        }
        shards.insert("orders".to_string(), order_shards);

        Self {
            shards: Mutex::new(shards),
        }
    }

    pub async fn get_shard(&self, shard_id: &str) -> Result<Shard> {
        let shards = self.shards.lock().unwrap();
        for table_shards in shards.values() {
            if let Some(shard) = table_shards.iter().find(|s| s.id == shard_id) {
                return Ok(shard.clone());
            }
        }
        Err(common::Error::Storage(format!("Shard {} not found", shard_id)))
    }

    pub async fn get_shards_for_table(&self, table_name: &str) -> Result<Vec<Shard>> {
        let shards = self.shards.lock().unwrap();
        Ok(shards.get(table_name).cloned().unwrap_or_default())
    }

    pub async fn get_table_shards(&self, table_name: &str) -> Result<Vec<Shard>> {
        self.get_shards_for_table(table_name).await
    }
}

// ============================================================================
// 分布式执行器
// ============================================================================

/// 分布式执行器
pub struct DistributedExecutor {
    /// 节点管理器
    node_manager: Arc<NodeManager>,
    /// 分片管理器
    #[allow(dead_code)]
    shard_manager: Arc<ShardManager>,
    /// 分布式事务管理器
    #[allow(dead_code)]
    transaction_manager: Arc<DistributedTransactionManager>,
    /// 执行统计
    stats: Arc<Mutex<DistributedExecutionStats>>,
}

impl DistributedExecutor {
    pub fn new() -> Self {
        Self {
            node_manager: Arc::new(NodeManager::new()),
            shard_manager: Arc::new(ShardManager::new()),
            transaction_manager: Arc::new(DistributedTransactionManager::new()),
            stats: Arc::new(Mutex::new(DistributedExecutionStats::new())),
        }
    }

    /// 执行分布式查询计划
    pub async fn execute_distributed_plan(&self, plan: DistributedQueryPlan) -> Result<DistributedQueryResult> {
        info!("Executing distributed query plan with {} fragments", plan.fragments.len());

        let mut stats = self.stats.lock().unwrap();
        stats.start_execution();

        // 1. 分析查询计划，确定执行策略
        let execution_strategy = self.analyze_execution_strategy(&plan).await?;

        // 2. 分配执行节点
        let node_assignments = self.assign_execution_nodes(&plan, &execution_strategy).await?;

        // 3. 并行执行查询片段
        let fragment_results = self.execute_fragments_parallel(&plan.fragments, &node_assignments).await?;

        // 4. 合并结果
        let final_result = self.merge_fragment_results(fragment_results).await?;

        stats.end_execution();
        debug!("Distributed query execution completed in {:?}", stats.execution_time());

        Ok(final_result)
    }

    /// 分析执行策略
    async fn analyze_execution_strategy(&self, plan: &DistributedQueryPlan) -> Result<ExecutionStrategy> {
        let mut strategy = ExecutionStrategy::new();

        for fragment in &plan.fragments {
            strategy.add_fragment(fragment.clone());
        }

        Ok(strategy)
    }

    /// 分配执行节点
    async fn assign_execution_nodes(
        &self,
        plan: &DistributedQueryPlan,
        _strategy: &ExecutionStrategy,
    ) -> Result<NodeAssignments> {
        let mut assignments = NodeAssignments::new();

        // 获取可用节点
        let available_nodes = self.node_manager.get_available_nodes().await?;

        // 为每个片段分配节点
        for fragment in &plan.fragments {
            let assigned_node = self.select_best_node(&fragment, &available_nodes).await?;
            assignments.assign_fragment(fragment.id.clone(), assigned_node);
        }

        Ok(assignments)
    }

    /// 选择最佳执行节点
    async fn select_best_node(
        &self,
        fragment: &QueryFragment,
        available_nodes: &[Node],
    ) -> Result<Node> {
        // 如果片段有目标节点，优先使用
        if let Some(target_node_id) = &fragment.target_node {
            for node in available_nodes {
                if node.id == *target_node_id {
                    return Ok(node.clone());
                }
            }
        }

        // 简单的负载均衡策略：选择负载最低的节点
        let mut best_node = available_nodes[0].clone();
        let mut min_load = f64::MAX;

        for node in available_nodes {
            let load = self.node_manager.get_node_load(&node.id).await?;
            if load < min_load {
                min_load = load;
                best_node = node.clone();
            }
        }

        Ok(best_node)
    }

    /// 并行执行查询片段
    async fn execute_fragments_parallel(
        &self,
        fragments: &[QueryFragment],
        assignments: &NodeAssignments,
    ) -> Result<Vec<FragmentResult>> {
        let mut handles = Vec::new();

        for fragment in fragments {
            let node_id = assignments.get_assigned_node(&fragment.id)?;
            let node = self.node_manager.get_node(&node_id).await?;

            let fragment_clone = fragment.clone();
            let node_clone = node.clone();

            let handle = tokio::spawn(async move {
                Self::execute_fragment_on_node(fragment_clone, node_clone).await
            });

            handles.push(handle);
        }

        let mut results = Vec::new();
        for handle in handles {
            let result = handle.await.map_err(|e| common::Error::Storage(e.to_string()))??;
            results.push(result);
        }

        Ok(results)
    }

    /// 在指定节点上执行查询片段
    async fn execute_fragment_on_node(fragment: QueryFragment, node: Node) -> Result<FragmentResult> {
        debug!("Executing fragment {} on node {}", fragment.id, node.id);

        // 模拟在节点上执行查询片段
        let result = FragmentResult {
            fragment_id: fragment.id,
            node_id: node.id,
            data: vec![
                vec!["1".to_string(), "Alice".to_string()],
                vec!["2".to_string(), "Bob".to_string()],
            ],
            execution_time: std::time::Duration::from_millis(10),
        };

        Ok(result)
    }

    /// 合并片段结果
    async fn merge_fragment_results(&self, results: Vec<FragmentResult>) -> Result<DistributedQueryResult> {
        let mut final_data = Vec::new();
        let mut fragment_count = 0;

        for result in &results {
            final_data.extend(result.data.clone());
            fragment_count += 1;
        }

        Ok(DistributedQueryResult {
            data: final_data,
            execution_time: std::time::Duration::from_secs(0), // TODO: 计算实际执行时间
            fragment_count,
        })
    }
}

/// 节点分配
#[derive(Debug, Clone)]
pub struct NodeAssignments {
    assignments: HashMap<String, String>, // fragment_id -> node_id
}

impl NodeAssignments {
    pub fn new() -> Self {
        Self {
            assignments: HashMap::new(),
        }
    }

    pub fn assign_fragment(&mut self, fragment_id: String, node: Node) {
        self.assignments.insert(fragment_id, node.id);
    }

    pub fn get_assigned_node(&self, fragment_id: &str) -> Result<String> {
        self.assignments
            .get(fragment_id)
            .cloned()
            .ok_or_else(|| common::Error::Storage("Fragment not assigned to any node".to_string()))
    }
}

// ============================================================================
// 分布式计划构建器
// ============================================================================

/// 分布式查询计划构建器
pub struct DistributedPlanBuilder {
    /// 分片管理器
    shard_manager: Arc<ShardManager>,
    /// 节点管理器
    #[allow(dead_code)]
    node_manager: Arc<NodeManager>,
}

impl DistributedPlanBuilder {
    pub fn new() -> Self {
        Self {
            shard_manager: Arc::new(ShardManager::new()),
            node_manager: Arc::new(NodeManager::new()),
        }
    }

    /// 构建分布式查询计划
    pub async fn build_distributed_plan(&self, sql: &str) -> Result<DistributedQueryPlan> {
        info!("Building distributed query plan for SQL: {}", sql);

        // 1. 解析SQL，识别表和分片
        let table_info = self.parse_sql_for_tables(sql).await?;

        // 2. 获取分片信息
        let shard_info = self.get_shard_info(&table_info).await?;

        // 3. 生成查询片段
        let fragments = self.generate_query_fragments(sql, &table_info, &shard_info).await?;

        // 4. 确定执行策略
        let execution_strategy = self.determine_execution_strategy(&fragments).await?;

        // 5. 计算预估成本
        let (estimated_cost, estimated_rows) = self.estimate_cost_and_rows(&fragments).await?;

        Ok(DistributedQueryPlan {
            fragments,
            execution_strategy,
            estimated_cost,
            estimated_rows,
            shard_info,
        })
    }

    /// 解析SQL，识别涉及的表
    async fn parse_sql_for_tables(&self, sql: &str) -> Result<Vec<TableInfo>> {
        // 简化的SQL解析，实际应该使用SQL解析器
        let mut tables = Vec::new();

        if sql.to_lowercase().contains("users") {
            tables.push(TableInfo {
                name: "users".to_string(),
                alias: None,
                shard_key: Some("id".to_string()),
            });
        }

        if sql.to_lowercase().contains("orders") {
            tables.push(TableInfo {
                name: "orders".to_string(),
                alias: None,
                shard_key: Some("user_id".to_string()),
            });
        }

        Ok(tables)
    }

    /// 获取分片信息
    async fn get_shard_info(&self, table_info: &[TableInfo]) -> Result<ShardInfo> {
        let mut shard_info = ShardInfo::new();

        for table in table_info {
            let shards = self.shard_manager.get_table_shards(&table.name).await?;
            let shard_ids: Vec<String> = shards.iter().map(|s| s.id.clone()).collect();
            shard_info.add_shard_mapping(table.name.clone(), shard_ids);

            // 添加分片分布
            for shard in shards {
                shard_info.add_shard_distribution(shard.id.clone(), shard.node_id.clone());
            }
        }

        Ok(shard_info)
    }

    /// 生成查询片段
    async fn generate_query_fragments(
        &self,
        sql: &str,
        table_info: &[TableInfo],
        shard_info: &ShardInfo,
    ) -> Result<Vec<QueryFragment>> {
        let mut fragments = Vec::new();

        for table in table_info {
            if let Some(shard_ids) = shard_info.get_table_shards(&table.name) {
                for (index, shard_id) in shard_ids.iter().enumerate() {
                    let fragment = QueryFragment {
                        id: format!("fragment-{}-{}", table.name, index),
                        execution_type: ExecutionType::Distributed,
                        shard_id: Some(shard_id.clone()),
                        sql: self.generate_shard_sql(sql, &table.name, shard_id).await?,
                        dependencies: Vec::new(),
                        target_node: shard_info.get_shard_node(shard_id).cloned(),
                        estimated_cost: 1.0,
                        estimated_rows: 1000,
                    };
                    fragments.push(fragment);
                }
            }
        }

        // 如果有多个表，生成连接片段
        if table_info.len() > 1 {
            let join_fragment = QueryFragment {
                id: "join-fragment".to_string(),
                execution_type: ExecutionType::CrossNodeJoin,
                shard_id: None,
                sql: sql.to_string(),
                dependencies: fragments.iter().map(|f| f.id.clone()).collect(),
                target_node: None,
                estimated_cost: 5.0,
                estimated_rows: 5000,
            };
            fragments.push(join_fragment);
        }

        Ok(fragments)
    }

    /// 生成分片SQL
    async fn generate_shard_sql(&self, _original_sql: &str, table_name: &str, shard_id: &str) -> Result<String> {
        // 简化的分片SQL生成
        let shard_sql = format!(
            "SELECT * FROM {} WHERE {} >= {} AND {} < {}",
            table_name,
            "id", // 假设分片键是id
            shard_id.parse::<i32>().unwrap_or(0) * 1000,
            "id",
            (shard_id.parse::<i32>().unwrap_or(0) + 1) * 1000,
        );
        Ok(shard_sql)
    }

    /// 确定执行策略
    async fn determine_execution_strategy(&self, fragments: &[QueryFragment]) -> Result<ExecutionStrategy> {
        let mut strategy = ExecutionStrategy::new();

        for fragment in fragments {
            strategy.add_fragment(fragment.clone());
        }

        Ok(strategy)
    }

    /// 预估成本和行数
    async fn estimate_cost_and_rows(&self, fragments: &[QueryFragment]) -> Result<(f64, u64)> {
        let mut total_cost = 0.0;
        let mut total_rows = 0;

        for fragment in fragments {
            total_cost += fragment.estimated_cost;
            total_rows += fragment.estimated_rows;
        }

        Ok((total_cost, total_rows))
    }
}

/// 表信息
#[derive(Debug, Clone)]
pub struct TableInfo {
    pub name: String,
    pub alias: Option<String>,
    pub shard_key: Option<String>,
}

// ============================================================================
// 分布式事务管理
// ============================================================================

/// 分布式事务管理器
pub struct DistributedTransactionManager {
    transactions: Mutex<HashMap<String, DistributedTransaction>>,
}

impl DistributedTransactionManager {
    pub fn new() -> Self {
        Self {
            transactions: Mutex::new(HashMap::new()),
        }
    }

    pub async fn begin_transaction(&self) -> Result<String> {
        let transaction_id = uuid::Uuid::new_v4().to_string();
        let transaction = DistributedTransaction {
            id: transaction_id.clone(),
            status: TransactionStatus::Active,
            participants: Vec::new(),
            start_time: std::time::Instant::now(),
        };

        let mut transactions = self.transactions.lock().unwrap();
        transactions.insert(transaction_id.clone(), transaction);

        Ok(transaction_id)
    }

    pub async fn commit_transaction(&self, transaction_id: &str) -> Result<()> {
        let mut transactions = self.transactions.lock().unwrap();
        if let Some(transaction) = transactions.get_mut(transaction_id) {
            transaction.status = TransactionStatus::Committed;
        }
        Ok(())
    }

    pub async fn rollback_transaction(&self, transaction_id: &str) -> Result<()> {
        let mut transactions = self.transactions.lock().unwrap();
        if let Some(transaction) = transactions.get_mut(transaction_id) {
            transaction.status = TransactionStatus::RolledBack;
        }
        Ok(())
    }
}

/// 分布式事务
#[derive(Debug, Clone)]
pub struct DistributedTransaction {
    pub id: String,
    pub status: TransactionStatus,
    pub participants: Vec<String>,
    pub start_time: std::time::Instant,
}

/// 事务状态
#[derive(Debug, Clone)]
pub enum TransactionStatus {
    Active,
    Committed,
    RolledBack,
}

// ============================================================================
// 结果和统计
// ============================================================================

/// 片段执行结果
#[derive(Debug, Clone)]
pub struct FragmentResult {
    pub fragment_id: String,
    pub node_id: String,
    pub data: Vec<Vec<String>>,
    pub execution_time: std::time::Duration,
}

/// 分布式查询结果
#[derive(Debug, Clone)]
pub struct DistributedQueryResult {
    pub data: Vec<Vec<String>>,
    pub execution_time: std::time::Duration,
    pub fragment_count: usize,
}

/// 分布式执行统计
#[derive(Debug, Clone)]
pub struct DistributedExecutionStats {
    start_time: std::time::Instant,
    end_time: Option<std::time::Instant>,
    #[allow(dead_code)]
    total_fragments: u64,
    #[allow(dead_code)]
    total_nodes: u64,
}

impl DistributedExecutionStats {
    pub fn new() -> Self {
        Self {
            start_time: std::time::Instant::now(),
            end_time: None,
            total_fragments: 0,
            total_nodes: 0,
        }
    }

    pub fn start_execution(&mut self) {
        self.start_time = std::time::Instant::now();
    }

    pub fn end_execution(&mut self) {
        self.end_time = Some(std::time::Instant::now());
    }

    pub fn execution_time(&self) -> std::time::Duration {
        self.end_time.unwrap_or_else(std::time::Instant::now) - self.start_time
    }
}

// ============================================================================
// 测试模块
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execution_strategy_new() {
        let strategy = ExecutionStrategy::new();
        assert!(strategy.local_fragments.is_empty());
        assert!(strategy.distributed_fragments.is_empty());
        assert!(strategy.cross_node_joins.is_empty());
        assert!(strategy.aggregate_fragments.is_empty());
    }

    #[test]
    fn test_shard_info_new() {
        let shard_info = ShardInfo::new();
        assert!(shard_info.shard_mapping.is_empty());
        assert!(shard_info.shard_distribution.is_empty());
    }

    #[test]
    fn test_distributed_executor_new() {
        let executor = DistributedExecutor::new();
        assert!(matches!(executor, DistributedExecutor { .. }));
    }

    #[tokio::test]
    async fn test_execute_distributed_plan() {
        let executor = DistributedExecutor::new();

        let fragment = QueryFragment {
            id: "fragment-1".to_string(),
            execution_type: ExecutionType::Local,
            shard_id: Some("shard-0".to_string()),
            sql: "SELECT * FROM users WHERE id < 1000".to_string(),
            dependencies: Vec::new(),
            target_node: None,
            estimated_cost: 1.0,
            estimated_rows: 1000,
        };

        let plan = DistributedQueryPlan {
            fragments: vec![fragment],
            execution_strategy: ExecutionStrategy::new(),
            estimated_cost: 0.0,
            estimated_rows: 1000,
            shard_info: ShardInfo::new(),
        };

        let result = executor.execute_distributed_plan(plan).await.unwrap();
        assert!(!result.data.is_empty());
        assert_eq!(result.fragment_count, 1);
    }

    #[tokio::test]
    async fn test_node_manager() {
        let node_manager = NodeManager::new();
        let nodes = node_manager.get_available_nodes().await.unwrap();
        assert!(!nodes.is_empty());
    }

    #[tokio::test]
    async fn test_shard_manager() {
        let shard_manager = ShardManager::new();
        let shards = shard_manager.get_shards_for_table("users").await.unwrap();
        assert_eq!(shards.len(), 4);
    }

    #[tokio::test]
    async fn test_distributed_plan_builder() {
        let builder = DistributedPlanBuilder::new();
        let sql = "SELECT * FROM users WHERE id < 1000";

        let plan = builder.build_distributed_plan(sql).await.unwrap();
        assert!(!plan.fragments.is_empty());
        assert!(plan.estimated_cost > 0.0);
        assert!(plan.estimated_rows > 0);
    }

    #[tokio::test]
    async fn test_parse_sql_for_tables() {
        let builder = DistributedPlanBuilder::new();
        let sql = "SELECT * FROM users JOIN orders ON users.id = orders.user_id";

        let tables = builder.parse_sql_for_tables(sql).await.unwrap();
        assert_eq!(tables.len(), 2);
        assert!(tables.iter().any(|t| t.name == "users"));
        assert!(tables.iter().any(|t| t.name == "orders"));
    }
}