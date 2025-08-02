//! 重试策略实现
//!
//! 提供可配置的重试机制

use std::time::Duration;
use tracing::warn;

/// 重试策略
pub struct RetryPolicy {
    pub max_retries: u32,
    pub retry_delay_ms: u64,
    pub backoff_multiplier: f64,
    pub max_delay_ms: u64,
}

impl RetryPolicy {
    /// 创建新的重试策略
    pub fn new(max_retries: u32, retry_delay_ms: u64) -> Self {
        Self {
            max_retries,
            retry_delay_ms,
            backoff_multiplier: 2.0,
            max_delay_ms: 30000, // 30秒
        }
    }

    /// 计算重试延迟
    pub fn calculate_delay(&self, attempt: u32) -> Duration {
        let delay_ms = (self.retry_delay_ms as f64 * self.backoff_multiplier.powi(attempt as i32))
            .min(self.max_delay_ms as f64) as u64;
        Duration::from_millis(delay_ms)
    }

    /// 是否应该重试
    pub fn should_retry(&self, attempt: u32, error: &str) -> bool {
        if attempt >= self.max_retries {
            return false;
        }

        // 根据错误类型决定是否重试
        let retryable_errors = [
            "timeout",
            "connection",
            "network",
            "temporary",
            "unavailable",
        ];

        let error_lower = error.to_lowercase();
        retryable_errors.iter().any(|&retryable| error_lower.contains(retryable))
    }

    /// 记录重试信息
    pub fn log_retry(&self, attempt: u32, error: &str) {
        if attempt == 0 {
            warn!("Operation failed, starting retry: {}", error);
        } else {
            warn!("Retry attempt {}/{} failed: {}", attempt + 1, self.max_retries, error);
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(3, 100)
    }
}