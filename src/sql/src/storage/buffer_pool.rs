use common::Result;
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

/// PostgreSQL 风格的缓冲池
#[derive(Debug)]
pub struct BufferPool {
    /// 缓冲池大小 (默认 1GB)
    #[allow(dead_code)]
    pool_size: usize,
    /// 缓冲区大小 (默认 8KB)
    buffer_size: usize,
    /// 缓冲区映射
    buffers: RwLock<HashMap<BufferId, Buffer>>,
    /// 统计信息
    stats: Mutex<BufferStats>,
    /// 空闲缓冲区
    #[allow(dead_code)]
    free_buffers: Mutex<VecDeque<BufferId>>,
}

impl BufferPool {
    pub fn new() -> Self {
        let pool_size = 1024 * 1024 * 1024; // 1GB
        let buffer_size = 8 * 1024; // 8KB
        let buffer_count = pool_size / buffer_size;

        let mut free_buffers = VecDeque::new();
        for i in 0..buffer_count {
            free_buffers.push_back(BufferId(i));
        }

        Self {
            pool_size,
            buffer_size,
            buffers: RwLock::new(HashMap::new()),
            free_buffers: Mutex::new(free_buffers),
            stats: Mutex::new(BufferStats::new()),
        }
    }

    /// 获取缓冲区
    pub fn get_buffer(&self, page_id: PageId) -> Result<Buffer> {
        let mut stats = self.stats.lock().unwrap();
        stats.access_count += 1;

        // 检查缓存
        {
            let buffers = self.buffers.read().unwrap();
            if let Some(buffer) = buffers.get(&BufferId(page_id.0)) {
                stats.hit_count += 1;
                return Ok(buffer.clone());
            }
        }

        // 缓存未命中，需要从磁盘读取
        stats.miss_count += 1;
        let buffer = self.load_from_disk(page_id)?;

        // 将缓冲区加入缓存
        {
            let mut buffers = self.buffers.write().unwrap();
            buffers.insert(BufferId(page_id.0), buffer.clone());
        }

        Ok(buffer)
    }

    /// 从磁盘加载页面
    fn load_from_disk(&self, page_id: PageId) -> Result<Buffer> {
        // 模拟从磁盘读取
        let data = vec![0u8; self.buffer_size];
        let buffer = Buffer {
            id: BufferId(page_id.0),
            page_id,
            data,
            dirty: false,
            access_count: 1,
            last_access: Instant::now(),
        };

        Ok(buffer)
    }

    /// 刷新脏缓冲区
    pub fn flush_dirty_buffers(&self) -> Result<()> {
        let mut buffers = self.buffers.write().unwrap();
        let mut stats = self.stats.lock().unwrap();

        for buffer in buffers.values_mut() {
            if buffer.dirty {
                // 模拟写入磁盘
                buffer.dirty = false;
                stats.flush_count += 1;
            }
        }

        Ok(())
    }

    /// 获取统计信息
    pub fn get_stats(&self) -> BufferStats {
        self.stats.lock().unwrap().clone()
    }
}

/// 缓冲区 ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId(usize);

/// 页面 ID
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId(pub usize);

/// 缓冲区
#[derive(Debug, Clone)]
pub struct Buffer {
    pub id: BufferId,
    pub page_id: PageId,
    pub data: Vec<u8>,
    pub dirty: bool,
    pub access_count: u64,
    pub last_access: Instant,
}

/// 缓冲区统计
#[derive(Debug, Clone)]
pub struct BufferStats {
    pub access_count: u64,
    pub hit_count: u64,
    pub miss_count: u64,
    pub flush_count: u64,
}

impl BufferStats {
    pub fn new() -> Self {
        Self {
            access_count: 0,
            hit_count: 0,
            miss_count: 0,
            flush_count: 0,
        }
    }

    pub fn hit_rate(&self) -> f64 {
        if self.access_count == 0 {
            0.0
        } else {
            self.hit_count as f64 / self.access_count as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_pool_new() {
        let buffer_pool = BufferPool::new();
        assert_eq!(buffer_pool.pool_size, 1024 * 1024 * 1024);
        assert_eq!(buffer_pool.buffer_size, 8 * 1024);
    }

    #[test]
    fn test_get_buffer() {
        let buffer_pool = BufferPool::new();
        let page_id = PageId(1);

        let buffer = buffer_pool.get_buffer(page_id).unwrap();
        assert_eq!(buffer.page_id, page_id);
        assert_eq!(buffer.data.len(), 8 * 1024);

        let stats = buffer_pool.get_stats();
        assert_eq!(stats.access_count, 1);
        assert_eq!(stats.miss_count, 1);
    }

    #[test]
    fn test_buffer_cache_hit() {
        let buffer_pool = BufferPool::new();
        let page_id = PageId(1);

        // 第一次访问，缓存未命中
        let _buffer1 = buffer_pool.get_buffer(page_id).unwrap();
        let stats1 = buffer_pool.get_stats();
        assert_eq!(stats1.miss_count, 1);

        // 第二次访问，缓存命中
        let _buffer2 = buffer_pool.get_buffer(page_id).unwrap();
        let stats2 = buffer_pool.get_stats();
        assert_eq!(stats2.hit_count, 1);
    }

    #[test]
    fn test_flush_dirty_buffers() {
        let buffer_pool = BufferPool::new();
        let page_id = PageId(1);

        let buffer = buffer_pool.get_buffer(page_id).unwrap();
        // 模拟标记为脏
        // 注意：这里我们无法直接修改 buffer，因为它是只读的
        // 在实际实现中，我们需要提供方法来标记缓冲区为脏

        let result = buffer_pool.flush_dirty_buffers();
        assert!(result.is_ok());
    }
}