//! 测试用例模块
//! 
//! 定义测试用例相关的数据结构和功能

use std::time::Duration;
use serde::{Deserialize, Serialize};
use anyhow::Result;

use crate::{TestCase, ExpectedResult, ValidationType, PerformanceThreshold};

/// 测试用例加载器
pub struct TestCaseLoader;

impl TestCaseLoader {
    /// 从文件加载测试用例
    pub fn load_from_file(file_path: &str) -> Result<Vec<TestCase>> {
        let content = std::fs::read_to_string(file_path)?;
        Self::parse_test_cases(&content)
    }
    
    /// 从字符串解析测试用例
    pub fn parse_test_cases(content: &str) -> Result<Vec<TestCase>> {
        let mut test_cases = Vec::new();
        let lines: Vec<&str> = content.lines().collect();
        
        let mut i = 0;
        while i < lines.len() {
            if lines[i].trim().starts_with("--") {
                // 找到测试用例开始
                if let Some(test_case) = Self::parse_single_test_case(&lines, &mut i)? {
                    test_cases.push(test_case);
                }
            } else {
                i += 1;
            }
        }
        
        Ok(test_cases)
    }
    
    /// 解析单个测试用例
    fn parse_single_test_case(lines: &[&str], index: &mut usize) -> Result<Option<TestCase>> {
        let start_line = lines[*index];
        
        // 解析测试名称
        let test_name = if start_line.contains("--") {
            start_line.trim_start_matches("--").trim().to_string()
        } else {
            return Ok(None);
        };
        
        *index += 1;
        
        let mut sql = String::new();
        let mut expected_data = Vec::new();
        let mut expected_columns = Vec::new();
        let mut expected_row_count = None;
        let mut validation_type = ValidationType::ExactMatch;
        let mut tags = Vec::new();
        let mut timeout_seconds = 30u64;
        
        // 解析 SQL 和期望结果
        while *index < lines.len() {
            let line = lines[*index].trim();
            
            if line.starts_with("--") && !line.starts_with("-- EXPECT") {
                // 新的测试用例开始
                break;
            }
            
            if line.starts_with("-- EXPECT") {
                // 解析期望结果
                if line.contains("ROWS") {
                    if let Some(count) = line.split_whitespace().last() {
                        expected_row_count = count.parse().ok();
                    }
                } else if line.contains("COLUMNS") {
                    let columns = line.split_whitespace().skip(2).map(|s| s.to_string()).collect();
                    expected_columns = columns;
                } else if line.contains("DATA") {
                    // 解析期望数据
                    *index += 1;
                    while *index < lines.len() {
                        let data_line = lines[*index].trim();
                        if data_line.is_empty() || data_line.starts_with("--") {
                            break;
                        }
                        let row: Vec<String> = data_line.split('|')
                            .map(|s| s.trim().to_string())
                            .collect();
                        expected_data.push(row);
                        *index += 1;
                    }
                    continue;
                } else if line.contains("PATTERN") {
                    validation_type = ValidationType::PatternMatch;
                } else if line.contains("FUZZY") {
                    validation_type = ValidationType::FuzzyMatch;
                } else if line.contains("PERFORMANCE") {
                    validation_type = ValidationType::PerformanceThreshold;
                } else if line.contains("ERROR") {
                    validation_type = ValidationType::ErrorCheck;
                }
            } else if line.starts_with("-- TAGS") {
                tags = line.split_whitespace().skip(2).map(|s| s.to_string()).collect();
            } else if line.starts_with("-- TIMEOUT") {
                if let Some(timeout) = line.split_whitespace().last() {
                    timeout_seconds = timeout.parse().unwrap_or(30);
                }
            } else if !line.is_empty() && !line.starts_with("--") {
                // SQL 语句
                sql.push_str(line);
                sql.push(' ');
            }
            
            *index += 1;
        }
        
        let expected_result = ExpectedResult {
            validation_type,
            data: if expected_data.is_empty() { None } else { Some(expected_data) },
            columns: if expected_columns.is_empty() { None } else { Some(expected_columns) },
            row_count: expected_row_count,
            performance_threshold: None,
            pattern: None,
        };
        
        Ok(Some(TestCase {
            name: test_name,
            description: format!("测试: {}", test_name),
            sql: sql.trim().to_string(),
            expected_result,
            tags,
            timeout_seconds,
            enabled: true,
        }))
    }
    
    /// 验证测试用例
    pub fn validate_test_case(test_case: &TestCase) -> Result<()> {
        if test_case.sql.is_empty() {
            return Err(anyhow::anyhow!("测试用例 SQL 不能为空"));
        }
        
        if test_case.name.is_empty() {
            return Err(anyhow::anyhow!("测试用例名称不能为空"));
        }
        
        if test_case.timeout_seconds == 0 {
            return Err(anyhow::anyhow!("超时时间必须大于 0"));
        }
        
        Ok(())
    }
    
    /// 生成测试用例模板
    pub fn generate_template(test_name: &str) -> String {
        format!(
            "-- {test_name}
-- 在这里编写 SQL 语句
SELECT * FROM test_table;

-- EXPECT ROWS 1
-- EXPECT COLUMNS id name age
-- EXPECT DATA
-- 1|Alice|25
-- 2|Bob|30

-- TAGS basic select
-- TIMEOUT 30
"
        )
    }
}

/// 测试用例验证器
pub struct TestCaseValidator;

impl TestCaseValidator {
    /// 验证测试用例集合
    pub fn validate_test_suite(test_cases: &[TestCase]) -> Result<Vec<String>> {
        let mut errors = Vec::new();
        
        for test_case in test_cases {
            if let Err(e) = TestCaseLoader::validate_test_case(test_case) {
                errors.push(format!("测试用例 '{}': {}", test_case.name, e));
            }
        }
        
        // 检查重复的测试名称
        let mut names = std::collections::HashSet::new();
        for test_case in test_cases {
            if !names.insert(&test_case.name) {
                errors.push(format!("重复的测试名称: {}", test_case.name));
            }
        }
        
        Ok(errors)
    }
    
    /// 检查测试用例的依赖关系
    pub fn check_dependencies(test_cases: &[TestCase]) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        
        for test_case in test_cases {
            if test_case.sql.to_uppercase().contains("DROP") {
                warnings.push(format!("测试用例 '{}' 包含 DROP 语句，可能影响其他测试", test_case.name));
            }
            
            if test_case.sql.to_uppercase().contains("TRUNCATE") {
                warnings.push(format!("测试用例 '{}' 包含 TRUNCATE 语句，可能影响其他测试", test_case.name));
            }
        }
        
        Ok(warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_parse_test_cases() {
        let content = r#"
-- test_select_basic
SELECT * FROM users WHERE id = 1;

-- EXPECT ROWS 1
-- EXPECT COLUMNS id name
-- EXPECT DATA
-- 1|Alice

-- TAGS basic select
-- TIMEOUT 30
"#;
        
        let test_cases = TestCaseLoader::parse_test_cases(content).unwrap();
        assert_eq!(test_cases.len(), 1);
        
        let test_case = &test_cases[0];
        assert_eq!(test_case.name, "test_select_basic");
        assert!(test_case.sql.contains("SELECT"));
        assert_eq!(test_case.tags, vec!["basic", "select"]);
        assert_eq!(test_case.timeout_seconds, 30);
    }
    
    #[test]
    fn test_validate_test_case() {
        let test_case = TestCase {
            name: "test".to_string(),
            description: "test".to_string(),
            sql: "SELECT 1".to_string(),
            expected_result: ExpectedResult {
                validation_type: ValidationType::ExactMatch,
                data: None,
                columns: None,
                row_count: Some(1),
                performance_threshold: None,
                pattern: None,
            },
            tags: vec![],
            timeout_seconds: 30,
            enabled: true,
        };
        
        let result = TestCaseLoader::validate_test_case(&test_case);
        assert!(result.is_ok());
    }
    
    #[test]
    fn test_generate_template() {
        let template = TestCaseLoader::generate_template("test_name");
        assert!(template.contains("test_name"));
        assert!(template.contains("SELECT"));
        assert!(template.contains("EXPECT"));
    }
} 