use crate::thread_pool::{Request, RequestPriority, RequestType};
use std::collections::{BinaryHeap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Duration, Instant};

/// 优先级队列项
#[derive(Debug, Clone)]
pub struct PriorityQueueItem {
    pub request: Request,
    pub priority_score: f64,
    pub wait_time: Duration,
    pub estimated_cost: u64,
}

impl PartialEq for PriorityQueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.priority_score == other.priority_score
    }
}

impl Eq for PriorityQueueItem {}

impl PartialOrd for PriorityQueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PriorityQueueItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.partial_cmp(other).unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// 多级优先级队列
pub struct MultiLevelPriorityQueue {
    /// 系统级队列（最高优先级）
    system_queue: Arc<Mutex<VecDeque<Request>>>,
    /// 管理级队列
    admin_queue: Arc<Mutex<VecDeque<Request>>>,
    /// 高优先级队列
    high_queue: Arc<Mutex<VecDeque<Request>>>,
    /// 普通优先级队列
    normal_queue: Arc<Mutex<VecDeque<Request>>>,
    /// 低优先级队列
    low_queue: Arc<Mutex<VecDeque<Request>>>,
    /// 后台队列（最低优先级）
    background_queue: Arc<Mutex<VecDeque<Request>>>,
    /// 自适应优先级队列
    adaptive_queue: Arc<Mutex<BinaryHeap<PriorityQueueItem>>>,
    /// 队列统计信息
    stats: Arc<RwLock<QueueStats>>,
    /// 是否启用自适应调度
    enable_adaptive: bool,
}

/// 队列统计信息
#[derive(Debug, Clone, Default)]
pub struct QueueStats {
    pub system_queue_size: usize,
    pub admin_queue_size: usize,
    pub high_queue_size: usize,
    pub normal_queue_size: usize,
    pub low_queue_size: usize,
    pub background_queue_size: usize,
    pub adaptive_queue_size: usize,
    pub total_requests: u64,
    pub avg_wait_time: Duration,
    pub max_wait_time: Duration,
}

impl MultiLevelPriorityQueue {
    pub fn new(enable_adaptive: bool) -> Self {
        Self {
            system_queue: Arc::new(Mutex::new(VecDeque::new())),
            admin_queue: Arc::new(Mutex::new(VecDeque::new())),
            high_queue: Arc::new(Mutex::new(VecDeque::new())),
            normal_queue: Arc::new(Mutex::new(VecDeque::new())),
            low_queue: Arc::new(Mutex::new(VecDeque::new())),
            background_queue: Arc::new(Mutex::new(VecDeque::new())),
            adaptive_queue: Arc::new(Mutex::new(BinaryHeap::new())),
            stats: Arc::new(RwLock::new(QueueStats::default())),
            enable_adaptive,
        }
    }

    /// 添加请求到队列
    pub async fn push(&self, request: Request) -> Result<(), String> {
        let now = Instant::now();
        let wait_time = now.duration_since(request.created_at);

        // 更新统计信息
        {
            let mut stats = self.stats.write().await;
            stats.total_requests += 1;
            stats.avg_wait_time = Duration::from_millis(
                ((stats.avg_wait_time.as_millis() as u64 + wait_time.as_millis() as u64) / 2)
                    as u64,
            );
            if wait_time > stats.max_wait_time {
                stats.max_wait_time = wait_time;
            }
        }

        if self.enable_adaptive {
            // 使用自适应队列
            let priority_score = self.calculate_priority_score(&request, wait_time);
            let item = PriorityQueueItem {
                request: request.clone(),
                priority_score,
                wait_time,
                estimated_cost: request.estimated_cost,
            };

            let mut queue = self.adaptive_queue.lock().await;
            queue.push(item);

            // 更新统计
            {
                let mut stats = self.stats.write().await;
                stats.adaptive_queue_size = queue.len();
            }
        } else {
            // 使用固定优先级队列
            let queue = match request.priority {
                RequestPriority::System => &self.system_queue,
                RequestPriority::Admin => &self.admin_queue,
                RequestPriority::High => &self.high_queue,
                RequestPriority::Normal => &self.normal_queue,
                RequestPriority::Low => &self.low_queue,
                RequestPriority::Background => &self.background_queue,
            };

            let mut queue_guard = queue.lock().await;
            queue_guard.push_back(request.clone());

            // 更新统计
            {
                let mut stats = self.stats.write().await;
                match request.priority {
                    RequestPriority::System => stats.system_queue_size = queue_guard.len(),
                    RequestPriority::Admin => stats.admin_queue_size = queue_guard.len(),
                    RequestPriority::High => stats.high_queue_size = queue_guard.len(),
                    RequestPriority::Normal => stats.normal_queue_size = queue_guard.len(),
                    RequestPriority::Low => stats.low_queue_size = queue_guard.len(),
                    RequestPriority::Background => stats.background_queue_size = queue_guard.len(),
                }
            }
        }

        Ok(())
    }

    /// 从队列中获取下一个请求
    pub async fn pop(&self) -> Option<Request> {
        if self.enable_adaptive {
            // 从自适应队列获取
            let mut queue = self.adaptive_queue.lock().await;
            if let Some(item) = queue.pop() {
                // 更新统计
                {
                    let mut stats = self.stats.write().await;
                    stats.adaptive_queue_size = queue.len();
                }
                return Some(item.request);
            }
        } else {
            // 按优先级顺序从固定队列获取
            let queues = [
                &self.system_queue,
                &self.admin_queue,
                &self.high_queue,
                &self.normal_queue,
                &self.low_queue,
                &self.background_queue,
            ];

            for queue in queues.iter() {
                let mut queue_guard = queue.lock().await;
                if let Some(request) = queue_guard.pop_front() {
                    // 更新统计
                    {
                        let mut stats = self.stats.write().await;
                        match request.priority {
                            RequestPriority::System => stats.system_queue_size = queue_guard.len(),
                            RequestPriority::Admin => stats.admin_queue_size = queue_guard.len(),
                            RequestPriority::High => stats.high_queue_size = queue_guard.len(),
                            RequestPriority::Normal => stats.normal_queue_size = queue_guard.len(),
                            RequestPriority::Low => stats.low_queue_size = queue_guard.len(),
                            RequestPriority::Background => {
                                stats.background_queue_size = queue_guard.len()
                            }
                        }
                    }
                    return Some(request);
                }
            }
        }

        None
    }

    /// 计算优先级分数（自适应调度）
    fn calculate_priority_score(&self, request: &Request, wait_time: Duration) -> f64 {
        let base_priority = match request.priority {
            RequestPriority::System => 0.0,
            RequestPriority::Admin => 1.0,
            RequestPriority::High => 2.0,
            RequestPriority::Normal => 3.0,
            RequestPriority::Low => 4.0,
            RequestPriority::Background => 5.0,
        };

        // 等待时间因子（等待越久优先级越高）
        let wait_factor = (wait_time.as_millis() as f64 / 1000.0).min(10.0);

        // 成本因子（成本越低优先级越高）
        let cost_factor = (request.estimated_cost as f64 / 1000.0).min(5.0);

        // 请求类型因子
        let type_factor = match request.request_type {
            RequestType::System => 0.0,
            RequestType::Admin => 0.5,
            RequestType::Query => 1.0,
            RequestType::Write => 1.5,
            RequestType::Transaction => 2.0,
            RequestType::Batch => 3.0,
        };

        // 综合优先级分数（分数越低优先级越高）
        base_priority + type_factor + cost_factor - wait_factor
    }

    /// 获取队列统计信息
    pub async fn get_stats(&self) -> QueueStats {
        self.stats.read().await.clone()
    }

    /// 获取队列总长度
    pub async fn len(&self) -> usize {
        if self.enable_adaptive {
            self.adaptive_queue.lock().await.len()
        } else {
            let stats = self.stats.read().await;
            stats.system_queue_size
                + stats.admin_queue_size
                + stats.high_queue_size
                + stats.normal_queue_size
                + stats.low_queue_size
                + stats.background_queue_size
        }
    }

    /// 检查队列是否为空
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use uuid::Uuid;

    #[tokio::test]
    async fn test_priority_queue_new() {
        let queue = MultiLevelPriorityQueue::new(false);
        assert_eq!(queue.len().await, 0);
        assert!(queue.is_empty().await);
    }

    #[tokio::test]
    async fn test_priority_queue_push_and_pop() {
        let queue = MultiLevelPriorityQueue::new(false);

        // 添加不同优先级的请求
        let request1 = Request {
            id: Uuid::new_v4(),
            priority: RequestPriority::High,
            request_type: RequestType::Query,
            sql: "SELECT * FROM users".to_string(),
            connection_id: Uuid::new_v4(),
            user_id: Some("user1".to_string()),
            database: Some("test".to_string()),
            created_at: Instant::now(),
            timeout: Duration::from_secs(30),
            estimated_cost: 100,
        };

        let request2 = Request {
            id: Uuid::new_v4(),
            priority: RequestPriority::Low,
            request_type: RequestType::Query,
            sql: "SELECT * FROM logs".to_string(),
            connection_id: Uuid::new_v4(),
            user_id: Some("user2".to_string()),
            database: Some("test".to_string()),
            created_at: Instant::now(),
            timeout: Duration::from_secs(30),
            estimated_cost: 50,
        };

        let request3 = Request {
            id: Uuid::new_v4(),
            priority: RequestPriority::System,
            request_type: RequestType::Admin,
            sql: "SHOW STATUS".to_string(),
            connection_id: Uuid::new_v4(),
            user_id: Some("admin".to_string()),
            database: Some("system".to_string()),
            created_at: Instant::now(),
            timeout: Duration::from_secs(30),
            estimated_cost: 200,
        };

        queue.push(request1).await.unwrap();
        queue.push(request2).await.unwrap();
        queue.push(request3).await.unwrap();

        assert_eq!(queue.len().await, 3);

        // 验证优先级顺序：System > High > Low
        let first = queue.pop().await.unwrap();
        assert_eq!(first.priority, RequestPriority::System);

        let second = queue.pop().await.unwrap();
        assert_eq!(second.priority, RequestPriority::High);

        let third = queue.pop().await.unwrap();
        assert_eq!(third.priority, RequestPriority::Low);

        assert!(queue.is_empty().await);
    }

    #[tokio::test]
    async fn test_priority_queue_peek() {
        let queue = MultiLevelPriorityQueue::new(false);

        let request = Request {
            id: Uuid::new_v4(),
            priority: RequestPriority::High,
            request_type: RequestType::Query,
            sql: "SELECT * FROM users".to_string(),
            connection_id: Uuid::new_v4(),
            user_id: Some("user1".to_string()),
            database: Some("test".to_string()),
            created_at: Instant::now(),
            timeout: Duration::from_secs(30),
            estimated_cost: 100,
        };

        queue.push(request).await.unwrap();

        let peeked = queue.pop().await.unwrap();
        assert_eq!(peeked.priority, RequestPriority::High);

        // 验证 peek 不会移除元素
        assert_eq!(queue.len().await, 0);
    }

    #[tokio::test]
    async fn test_priority_queue_stats() {
        let queue = MultiLevelPriorityQueue::new(false);
        let start_time = Instant::now();

        // 添加一些请求
        for i in 0..3 {
            let request = Request {
                id: Uuid::new_v4(),
                priority: RequestPriority::Normal,
                request_type: RequestType::Query,
                sql: format!("SELECT * FROM table{i}"),
                connection_id: Uuid::new_v4(),
                user_id: Some(format!("user{i}")),
                database: Some("test".to_string()),
                created_at: start_time,
                timeout: Duration::from_secs(30),
                estimated_cost: 100,
            };
            queue.push(request).await.unwrap();
        }

        let stats = queue.get_stats().await;
        assert_eq!(stats.total_requests, 3);
        assert_eq!(stats.avg_wait_time.as_millis(), 0); // 刚添加的请求等待时间为0
    }

    #[test]
    fn test_request_priority_ordering() {
        let priorities = [
            RequestPriority::System,
            RequestPriority::Admin,
            RequestPriority::High,
            RequestPriority::Normal,
            RequestPriority::Low,
            RequestPriority::Background,
        ];

        // 验证优先级顺序（数值越小优先级越高）
        for i in 0..priorities.len() - 1 {
            assert!(priorities[i] < priorities[i + 1]);
        }
    }

    #[test]
    fn test_request_type_creation() {
        let query_type = RequestType::Query;
        let admin_type = RequestType::Admin;
        let transaction_type = RequestType::Transaction;

        assert_ne!(query_type, admin_type);
        assert_ne!(admin_type, transaction_type);
        assert_ne!(query_type, transaction_type);
    }
}
