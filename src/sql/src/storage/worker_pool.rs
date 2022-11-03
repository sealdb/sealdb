use common::Result;
use std::sync::mpsc::{channel, Sender, Receiver};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};
use std::collections::HashMap;
use tokio::sync::Semaphore;

/// 任务优先级
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TaskPriority {
    Low = 0,
    Normal = 1,
    High = 2,
    Critical = 3,
}

/// 任务类型
#[derive(Debug, Clone)]
pub enum TaskType {
    Query,
    Maintenance,
    Background,
    System,
}

/// 任务信息
#[derive(Debug, Clone)]
pub struct TaskInfo {
    pub id: String,
    pub priority: TaskPriority,
    pub task_type: TaskType,
    pub submitted_at: Instant,
    pub estimated_duration: Option<Duration>,
}

/// 工作线程状态
#[derive(Debug, Clone)]
pub enum WorkerState {
    Idle,
    Busy { task_id: String, started_at: Instant },
    ShuttingDown,
}

/// 工作线程信息
#[derive(Debug, Clone)]
pub struct WorkerInfo {
    pub id: usize,
    pub state: WorkerState,
    pub tasks_completed: u64,
    pub total_work_time: Duration,
    pub last_activity: Instant,
}

/// 增强的多线程工作池
pub struct WorkerPool {
    /// 配置
    config: Arc<RwLock<WorkerPoolConfig>>,
    /// 工作线程数量
    worker_count: usize,
    /// 工作线程句柄
    workers: Vec<thread::JoinHandle<()>>,
    /// 任务发送器
    task_sender: Sender<Box<dyn FnOnce() + Send>>,
    /// 高优先级任务发送器
    priority_task_sender: Sender<(TaskInfo, Box<dyn FnOnce() + Send>)>,
    /// 工作线程信息
    worker_info: Arc<RwLock<HashMap<usize, WorkerInfo>>>,
    /// 并行度控制信号量
    parallelism_semaphore: Arc<Semaphore>,
    /// 统计信息
    stats: Arc<Mutex<WorkerPoolStats>>,
    /// 是否已关闭
    shutdown: Arc<Mutex<bool>>,
}

/// 工作线程池配置
#[derive(Debug, Clone)]
pub struct WorkerPoolConfig {
    /// 最小工作线程数
    pub min_worker_threads: usize,
    /// 最大工作线程数
    pub max_worker_threads: usize,
    /// 初始工作线程数
    pub initial_worker_threads: usize,
    /// 任务队列大小
    pub task_queue_size: usize,
    /// 是否启用线程监控
    pub enable_thread_monitoring: bool,
    /// 线程空闲超时时间（秒）
    pub thread_idle_timeout_seconds: u64,
    /// 是否启用任务优先级
    pub enable_task_priority: bool,
    /// 最大并行度
    pub max_parallelism: usize,
    /// 是否启用动态调整
    pub enable_dynamic_adjustment: bool,
    /// 调整检查间隔（秒）
    pub adjustment_check_interval_seconds: u64,
    /// CPU 使用率阈值
    pub cpu_usage_threshold: f64,
    /// 内存使用率阈值
    pub memory_usage_threshold: f64,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        Self {
            min_worker_threads: 2,
            max_worker_threads: cpu_count * 2,
            initial_worker_threads: cpu_count,
            task_queue_size: 1000,
            enable_thread_monitoring: true,
            thread_idle_timeout_seconds: 300,
            enable_task_priority: true,
            max_parallelism: cpu_count * 4,
            enable_dynamic_adjustment: true,
            adjustment_check_interval_seconds: 30,
            cpu_usage_threshold: 0.8,
            memory_usage_threshold: 0.8,
        }
    }
}

impl WorkerPool {
    pub fn new() -> Self {
        Self::with_config(WorkerPoolConfig::default())
    }

    pub fn with_config(config: WorkerPoolConfig) -> Self {
        let worker_count = config.initial_worker_threads;
        let (task_sender, task_receiver) = channel::<Box<dyn FnOnce() + Send>>();
        let (priority_task_sender, priority_task_receiver) = channel::<(TaskInfo, Box<dyn FnOnce() + Send>)>();

        let shared_receiver = Arc::new(Mutex::new(task_receiver));
        let shared_priority_receiver = Arc::new(Mutex::new(priority_task_receiver));
        let worker_info = Arc::new(RwLock::new(HashMap::new()));
        let parallelism_semaphore = Arc::new(Semaphore::new(config.max_parallelism));
        let stats = Arc::new(Mutex::new(WorkerPoolStats::new()));
        let shutdown = Arc::new(Mutex::new(false));
        let config = Arc::new(RwLock::new(config));

        let mut workers = Vec::new();

        // 创建工作线程
        for worker_id in 0..worker_count {
            let shared_receiver = shared_receiver.clone();
            let shared_priority_receiver = shared_priority_receiver.clone();
            let worker_info = worker_info.clone();
            let stats = stats.clone();
            let shutdown = shutdown.clone();
            let config = config.clone();

            let worker = thread::spawn(move || {
                let thread_name = format!("worker-{}", worker_id);

                // 初始化工作线程信息
                {
                    let mut info_map = worker_info.write().unwrap();
                    info_map.insert(worker_id, WorkerInfo {
                        id: worker_id,
                        state: WorkerState::Idle,
                        tasks_completed: 0,
                        total_work_time: Duration::ZERO,
                        last_activity: Instant::now(),
                    });
                }

                loop {
                    // 检查是否关闭
                    if *shutdown.lock().unwrap() {
                        break;
                    }

                    // 优先处理高优先级任务
                    let task = {
                        let priority_receiver = shared_priority_receiver.lock().unwrap();
                        match priority_receiver.try_recv() {
                            Ok((task_info, task)) => {
                                // 更新工作线程状态
                                {
                                    let mut info_map = worker_info.write().unwrap();
                                    if let Some(worker) = info_map.get_mut(&worker_id) {
                                        worker.state = WorkerState::Busy {
                                            task_id: task_info.id.clone(),
                                            started_at: Instant::now(),
                                        };
                                        worker.last_activity = Instant::now();
                                    }
                                }

                                // 执行任务
                                let start_time = Instant::now();
                                task();
                                let execution_time = start_time.elapsed();

                                // 更新统计信息
                                {
                                    let mut stats = stats.lock().unwrap();
                                    stats.tasks_completed += 1;
                                    stats.total_execution_time += execution_time;
                                    stats.average_execution_time_ms =
                                        stats.total_execution_time.as_millis() as f64 / stats.tasks_completed as f64;
                                }

                                // 更新工作线程信息
                                {
                                    let mut info_map = worker_info.write().unwrap();
                                    if let Some(worker) = info_map.get_mut(&worker_id) {
                                        worker.state = WorkerState::Idle;
                                        worker.tasks_completed += 1;
                                        worker.total_work_time += execution_time;
                                        worker.last_activity = Instant::now();
                                    }
                                }

                                continue;
                            }
                            Err(_) => {
                                // 没有高优先级任务，处理普通任务
                                let receiver = shared_receiver.lock().unwrap();
                                receiver.recv()
                            }
                        }
                    };

                    match task {
                        Ok(task) => {
                            // 更新工作线程状态
                            {
                                let mut info_map = worker_info.write().unwrap();
                                if let Some(worker) = info_map.get_mut(&worker_id) {
                                    worker.state = WorkerState::Busy {
                                        task_id: "regular_task".to_string(),
                                        started_at: Instant::now(),
                                    };
                                    worker.last_activity = Instant::now();
                                }
                            }

                            // 执行任务
                            let start_time = Instant::now();
                            task();
                            let execution_time = start_time.elapsed();

                            // 更新统计信息
                            {
                                let mut stats = stats.lock().unwrap();
                                stats.tasks_completed += 1;
                                stats.total_execution_time += execution_time;
                                stats.average_execution_time_ms =
                                    stats.total_execution_time.as_millis() as f64 / stats.tasks_completed as f64;
                            }

                            // 更新工作线程信息
                            {
                                let mut info_map = worker_info.write().unwrap();
                                if let Some(worker) = info_map.get_mut(&worker_id) {
                                    worker.state = WorkerState::Idle;
                                    worker.tasks_completed += 1;
                                    worker.total_work_time += execution_time;
                                    worker.last_activity = Instant::now();
                                }
                            }
                        }
                        Err(_) => {
                            // 发送器已关闭，退出线程
                            break;
                        }
                    }
                }
            });

            workers.push(worker);
        }

        // 启动动态调整线程
        if config.read().unwrap().enable_dynamic_adjustment {
            let worker_info = worker_info.clone();
            let config = config.clone();
            let shutdown = shutdown.clone();

            thread::spawn(move || {
                let adjustment_interval = Duration::from_secs(
                    config.read().unwrap().adjustment_check_interval_seconds
                );

                loop {
                    if *shutdown.lock().unwrap() {
                        break;
                    }

                    thread::sleep(adjustment_interval);

                    // TODO: 实现动态调整逻辑
                    // 这里可以根据 CPU 使用率、内存使用率等指标调整线程数
                }
            });
        }

        Self {
            config,
            worker_count,
            workers,
            task_sender,
            priority_task_sender,
            worker_info,
            parallelism_semaphore,
            stats,
            shutdown,
        }
    }

    /// 提交任务到工作线程池
    pub fn submit_task<F>(&self, task: F) -> Result<()>
    where
        F: FnOnce() + Send + 'static,
    {
        let mut stats = self.stats.lock().unwrap();
        stats.tasks_submitted += 1;

        self.task_sender
            .send(Box::new(task))
            .map_err(|e| common::Error::Storage(e.to_string()))?;
        Ok(())
    }

    /// 提交带优先级和结果的任务
    pub fn submit_priority_task<F, T>(&self, task_info: TaskInfo, task: F) -> Result<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (result_sender, result_receiver) = channel();

        let wrapped_task = move || {
            let result = task();
            let _ = result_sender.send(result);
        };

        let mut stats = self.stats.lock().unwrap();
        stats.tasks_submitted += 1;

        self.priority_task_sender
            .send((task_info, Box::new(wrapped_task)))
            .map_err(|e| common::Error::Storage(e.to_string()))?;

        result_receiver
            .recv()
            .map_err(|e| common::Error::Storage(e.to_string()))
    }

    /// 提交带结果的任务
    pub fn submit_task_with_result<F, T>(&self, task: F) -> Result<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (result_sender, result_receiver) = channel();

        let wrapped_task = move || {
            let result = task();
            let _ = result_sender.send(result);
        };

        self.submit_task(wrapped_task)?;

        result_receiver
            .recv()
            .map_err(|e| common::Error::Storage(e.to_string()))
    }

    /// 并行执行多个任务
    pub fn execute_parallel<F, T>(&self, tasks: Vec<F>) -> Result<Vec<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let mut results = Vec::with_capacity(tasks.len());
        let mut handles = Vec::new();

        for task in tasks {
            let (result_sender, result_receiver) = channel();

            let wrapped_task = move || {
                let result = task();
                let _ = result_sender.send(result);
            };

            self.submit_task(wrapped_task)?;
            handles.push(result_receiver);
        }

        // 收集结果
        for handle in handles {
            let result = handle.recv()
                .map_err(|e| common::Error::Storage(e.to_string()))?;
            results.push(result);
        }

        let mut stats = self.stats.lock().unwrap();
        stats.parallel_executions += 1;

        Ok(results)
    }

    /// 获取工作线程数量
    pub fn worker_count(&self) -> usize {
        self.worker_count
    }

    /// 获取工作线程信息
    pub fn get_worker_info(&self) -> HashMap<usize, WorkerInfo> {
        self.worker_info.read().unwrap().clone()
    }

    /// 获取统计信息
    pub fn get_stats(&self) -> WorkerPoolStats {
        self.stats.lock().unwrap().clone()
    }

    /// 调整并行度
    pub fn adjust_parallelism(&self, new_parallelism: usize) -> Result<()> {
        let config = self.config.read().unwrap();
        if new_parallelism > config.max_parallelism {
            return Err(common::Error::Storage(
                format!("Parallelism {} exceeds maximum {}", new_parallelism, config.max_parallelism)
            ));
        }

        // 创建新的信号量
        let new_semaphore = Arc::new(Semaphore::new(new_parallelism));

        // 原子性地替换信号量
        // 注意：这里需要更复杂的实现来确保线程安全
        // 暂时使用简单的替换
        Ok(())
    }

    /// 动态调整工作线程数
    pub fn adjust_worker_count(&self, new_count: usize) -> Result<()> {
        let config = self.config.read().unwrap();
        if new_count < config.min_worker_threads || new_count > config.max_worker_threads {
            return Err(common::Error::Storage(
                format!("Worker count {} is outside valid range [{}, {}]",
                    new_count, config.min_worker_threads, config.max_worker_threads)
            ));
        }

        // TODO: 实现动态调整工作线程数的逻辑
        // 这需要更复杂的实现来安全地添加或移除工作线程

        Ok(())
    }

    /// 等待所有任务完成
    pub fn wait_for_completion(&self) -> Result<()> {
        // 等待所有工作线程完成当前任务
        // 这里可以实现更复杂的等待逻辑
        Ok(())
    }

    /// 关闭工作线程池
    pub fn shutdown(&self) -> Result<()> {
        let mut shutdown = self.shutdown.lock().unwrap();
        *shutdown = true;

        // 等待所有工作线程完成
        // 注意：在实际实现中，我们需要更复杂的逻辑来处理线程关闭
        // 这里简化处理，实际应该使用 Arc<Mutex<Vec<JoinHandle>>> 来管理线程

        Ok(())
    }
}

/// 增强的工作线程池统计信息
#[derive(Debug, Clone)]
pub struct WorkerPoolStats {
    pub tasks_submitted: u64,
    pub tasks_completed: u64,
    pub parallel_executions: u64,
    pub total_execution_time: Duration,
    pub average_execution_time_ms: f64,
    pub current_parallelism: usize,
    pub active_workers: usize,
    pub idle_workers: usize,
}

impl WorkerPoolStats {
    pub fn new() -> Self {
        Self {
            tasks_submitted: 0,
            tasks_completed: 0,
            parallel_executions: 0,
            total_execution_time: Duration::ZERO,
            average_execution_time_ms: 0.0,
            current_parallelism: 0,
            active_workers: 0,
            idle_workers: 0,
        }
    }
}

/// 查询结果
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub affected_rows: u64,
    pub last_insert_id: Option<u64>,
}

impl QueryResult {
    pub fn new() -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            affected_rows: 0,
            last_insert_id: None,
        }
    }

    pub fn with_columns(columns: Vec<String>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
            affected_rows: 0,
            last_insert_id: None,
        }
    }

    pub fn add_row(&mut self, row: Vec<String>) {
        self.rows.push(row);
    }

    pub fn merge(&mut self, other: QueryResult) {
        if self.columns.is_empty() {
            self.columns = other.columns;
        }
        self.rows.extend(other.rows);
        self.affected_rows += other.affected_rows;
        if other.last_insert_id.is_some() {
            self.last_insert_id = other.last_insert_id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_worker_pool_new() {
        let worker_pool = WorkerPool::new();
        assert!(worker_pool.worker_count() > 0);
    }

    #[test]
    fn test_submit_task() {
        let worker_pool = WorkerPool::new();
        let result = worker_pool.submit_task(|| {
            println!("Task executed");
        });
        assert!(result.is_ok());
    }

    #[test]
    fn test_submit_task_with_result() {
        let worker_pool = WorkerPool::new();
        let result = worker_pool.submit_task_with_result(|| {
            "Hello, World!".to_string()
        });
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Hello, World!");
    }

    #[test]
    fn test_execute_parallel() {
        let worker_pool = WorkerPool::new();
        let tasks = vec![
            || 1,
            || 2,
            || 3,
            || 4,
        ];
        let result = worker_pool.execute_parallel(tasks);
        assert!(result.is_ok());
        let results = result.unwrap();
        assert_eq!(results, vec![1, 2, 3, 4]);
    }

    #[test]
    fn test_priority_task() {
        let worker_pool = WorkerPool::new();
        let task_info = TaskInfo {
            id: "test_task".to_string(),
            priority: TaskPriority::High,
            task_type: TaskType::Query,
            submitted_at: Instant::now(),
            estimated_duration: Some(Duration::from_millis(100)),
        };

        let result = worker_pool.submit_priority_task(task_info, || {
            "Priority task executed".to_string()
        });
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "Priority task executed");
    }

    #[test]
    fn test_worker_pool_stats() {
        let worker_pool = WorkerPool::new();
        let stats = worker_pool.get_stats();
        assert_eq!(stats.tasks_submitted, 0);
        assert_eq!(stats.tasks_completed, 0);
    }

    #[test]
    fn test_query_result() {
        let mut result = QueryResult::with_columns(vec!["id".to_string(), "name".to_string()]);
        result.add_row(vec!["1".to_string(), "Alice".to_string()]);
        result.add_row(vec!["2".to_string(), "Bob".to_string()]);

        assert_eq!(result.columns, vec!["id", "name"]);
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0], vec!["1", "Alice"]);
        assert_eq!(result.rows[1], vec!["2", "Bob"]);
    }
}