//! 结果验证器模块
//! 
//! 负责验证测试结果是否符合期望

use std::collections::HashMap;
use anyhow::Result;
use tracing::{debug, warn, error};

use crate::{
    QueryResult, ExpectedResult, ValidationType, ValidationConfig,
    PerformanceMetrics, PerformanceThreshold,
};

/// 结果验证器
pub struct ResultChecker {
    config: ValidationConfig,
}

impl ResultChecker {
    /// 创建新的结果验证器
    pub fn new(config: ValidationConfig) -> Self {
        Self { config }
    }
    
    /// 验证查询结果
    pub fn validate_result(&self, actual: &QueryResult, expected: &ExpectedResult) -> Result<bool> {
        debug!("验证结果: {}", actual.sql);
        
        match expected.validation_type {
            ValidationType::ExactMatch => self.validate_exact_match(actual, expected),
            ValidationType::FuzzyMatch => self.validate_fuzzy_match(actual, expected),
            ValidationType::PatternMatch => self.validate_pattern_match(actual, expected),
            ValidationType::PerformanceThreshold => self.validate_performance_threshold(actual, expected),
            ValidationType::ErrorCheck => self.validate_error_check(actual, expected),
        }
    }
    
    /// 精确匹配验证
    fn validate_exact_match(&self, actual: &QueryResult, expected: &ExpectedResult) -> Result<bool> {
        // 检查行数
        if let Some(expected_row_count) = expected.row_count {
            if actual.row_count != expected_row_count {
                warn!("行数不匹配: 期望 {}, 实际 {}", expected_row_count, actual.row_count);
                return Ok(false);
            }
        }
        
        // 检查列名
        if let Some(ref expected_columns) = expected.columns {
            if actual.columns != *expected_columns {
                warn!("列名不匹配: 期望 {:?}, 实际 {:?}", expected_columns, actual.columns);
                return Ok(false);
            }
        }
        
        // 检查数据
        if let Some(ref expected_data) = expected.data {
            if actual.data != *expected_data {
                warn!("数据不匹配");
                return Ok(false);
            }
        }
        
        Ok(true)
    }
    
    /// 模糊匹配验证
    fn validate_fuzzy_match(&self, actual: &QueryResult, expected: &ExpectedResult) -> Result<bool> {
        // 检查行数 (允许误差)
        if let Some(expected_row_count) = expected.row_count {
            let tolerance = (expected_row_count as f64 * self.config.tolerance) as usize;
            let min_count = expected_row_count.saturating_sub(tolerance);
            let max_count = expected_row_count + tolerance;
            
            if actual.row_count < min_count || actual.row_count > max_count {
                warn!("行数超出容差范围: 期望 {}±{}, 实际 {}", 
                      expected_row_count, tolerance, actual.row_count);
                return Ok(false);
            }
        }
        
        // 检查数据 (允许部分差异)
        if let Some(ref expected_data) = expected.data {
            let diff_count = self.calculate_data_difference(&actual.data, expected_data);
            if diff_count > self.config.max_diff_rows {
                warn!("数据差异过大: {} 行差异", diff_count);
                return Ok(false);
            }
        }
        
        Ok(true)
    }
    
    /// 模式匹配验证
    fn validate_pattern_match(&self, actual: &QueryResult, expected: &ExpectedResult) -> Result<bool> {
        if let Some(ref pattern) = expected.pattern {
            let regex = regex::Regex::new(pattern)?;
            
            // 检查 SQL 语句是否匹配模式
            if !regex.is_match(&actual.sql) {
                warn!("SQL 语句不匹配模式: {}", pattern);
                return Ok(false);
            }
            
            // 检查结果数据是否匹配模式
            for row in &actual.data {
                for cell in row {
                    if !regex.is_match(cell) {
                        warn!("数据不匹配模式: {}", pattern);
                        return Ok(false);
                    }
                }
            }
        }
        
        Ok(true)
    }
    
    /// 性能阈值验证
    fn validate_performance_threshold(&self, actual: &QueryResult, expected: &ExpectedResult) -> Result<bool> {
        if let Some(ref threshold) = expected.performance_threshold {
            // 检查执行时间
            if actual.execution_time_ms > threshold.max_execution_time_ms {
                warn!("执行时间超出阈值: {}ms > {}ms", 
                      actual.execution_time_ms, threshold.max_execution_time_ms);
                return Ok(false);
            }
        }
        
        Ok(true)
    }
    
    /// 错误检查验证
    fn validate_error_check(&self, actual: &QueryResult, expected: &ExpectedResult) -> Result<bool> {
        // 检查是否有错误
        if let Some(ref error) = actual.error {
            if expected.pattern.is_some() {
                let regex = regex::Regex::new(&expected.pattern.as_ref().unwrap())?;
                if !regex.is_match(error) {
                    warn!("错误信息不匹配模式: {}", error);
                    return Ok(false);
                }
            }
            Ok(true)
        } else {
            warn!("期望错误但实际没有错误");
            Ok(false)
        }
    }
    
    /// 计算数据差异
    fn calculate_data_difference(&self, actual: &[Vec<String>], expected: &[Vec<String>]) -> usize {
        let mut diff_count = 0;
        
        for (i, row) in actual.iter().enumerate() {
            if i >= expected.len() {
                diff_count += 1;
                continue;
            }
            
            let expected_row = &expected[i];
            if row != expected_row {
                diff_count += 1;
            }
        }
        
        diff_count + expected.len().saturating_sub(actual.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_exact_match_validation() {
        let config = ValidationConfig {
            exact_match: true,
            case_sensitive: false,
            ignore_whitespace: true,
            tolerance: 0.01,
            max_diff_rows: 10,
        };
        
        let checker = ResultChecker::new(config);
        
        let actual = QueryResult {
            sql: "SELECT * FROM test".to_string(),
            data: vec![vec!["1".to_string(), "test".to_string()]],
            columns: vec!["id".to_string(), "name".to_string()],
            row_count: 1,
            execution_time_ms: 100,
            error: None,
        };
        
        let expected = ExpectedResult {
            validation_type: ValidationType::ExactMatch,
            data: Some(vec![vec!["1".to_string(), "test".to_string()]]),
            columns: Some(vec!["id".to_string(), "name".to_string()]),
            row_count: Some(1),
            performance_threshold: None,
            pattern: None,
        };
        
        let result = checker.validate_result(&actual, &expected).unwrap();
        assert!(result);
    }
} 