# ANTLR4 Parser Test Suite

这个测试套件专门用于测试ANTLR4解析器的功能完整性。

## 功能概述

`antlr4_parser_test` 程序提供了全面的ANTLR4解析器测试，包括：

### 1. 基本功能测试 (`test_basic_functionality`)
- 测试基本的SQL语句解析
- 验证解析器是否正确创建
- 检查AST生成是否成功

### 2. 高级功能测试 (`test_advanced_functionality`)
- 复杂SELECT语句（JOIN、GROUP BY、HAVING等）
- 子查询支持
- 聚合函数
- 复杂INSERT/UPDATE/DELETE语句
- 复杂CREATE TABLE语句
- 索引创建语句

### 3. 错误处理测试 (`test_error_handling`)
- 测试不完整SQL语句的错误检测
- 验证语法错误是否正确报告
- 检查错误位置信息是否准确

### 4. 性能测试 (`test_performance`)
- 测量解析速度
- 比较不同SQL语句的解析性能
- 提供性能基准数据

### 5. 解析器比较测试 (`test_parser_comparison`)
- 比较ANTLR4解析器与其他解析器的性能
- 功能兼容性测试
- 性能基准对比

### 6. 语法特性测试 (`test_syntax_features`)
- 测试各种SQL语法特性的支持
- 包括别名、条件、排序、分组等
- 验证语法扩展的兼容性

## 使用方法

### 编译
```bash
cd build
make antlr4_parser_test
```

### 运行
```bash
./examples/antlr4_parser_test
```

## 测试输出示例

```
=== ANTLR4 Parser Test Suite ===

=== Testing ANTLR4 Parser Basic Functionality ===
Parser name: Antlr4Parser
Parser available: Yes

Testing SQL: SELECT * FROM users
  ✓ Parse Success
  ✓ AST created successfully

Testing SQL: INSERT INTO users (name, age) VALUES ('John', 25)
  ✓ Parse Success
  ✓ AST created successfully

=== Testing ANTLR4 Parser Advanced Functionality ===
Testing Advanced SQL: SELECT u.id, u.name, COUNT(o.id) as order_count FROM users u LEFT JOIN orders o ON u.id = o.user_id WHERE u.age > 18 GROUP BY u.id, u.name HAVING COUNT(o.id) > 0 ORDER BY order_count DESC LIMIT 10
  ✓ Advanced Parse Success
  ✓ Advanced AST created successfully

=== Testing ANTLR4 Parser Error Handling ===
Testing Error SQL: SELECT * FROM
  ✓ Error correctly detected:
    Error: Expected table name at line 1, column 15

=== Testing ANTLR4 Parser Performance ===
Testing SQL: SELECT * FROM users WHERE age > 18
  Performance: 1000 iterations in 15000 microseconds
  Average: 15 microseconds per parse

=== Testing Parser Comparison ===
Testing Antlr4Parser
  ✓ Functionality: PASS
  Performance: 100 iterations in 1500 microseconds
  Average: 15 microseconds per parse

=== Testing ANTLR4 Parser Syntax Features ===
Testing Basic SELECT: SELECT * FROM users
  ✓ Basic SELECT supported

Testing Column Aliases: SELECT id as user_id, name as user_name FROM users
  ✓ Column Aliases supported

=== ANTLR4 Parser Test Complete ===
```

## 测试覆盖范围

### SQL语句类型
- ✅ SELECT (基本、复杂、子查询)
- ✅ INSERT (单行、多行)
- ✅ UPDATE (条件更新)
- ✅ DELETE (条件删除)
- ✅ CREATE TABLE (基本、复杂约束)
- ✅ DROP TABLE
- ✅ CREATE INDEX

### SQL特性
- ✅ 表别名和列别名
- ✅ WHERE条件（AND、OR、比较运算符）
- ✅ ORDER BY (ASC/DESC)
- ✅ GROUP BY 和 HAVING
- ✅ LIMIT 和 OFFSET
- ✅ JOIN (INNER、LEFT、RIGHT)
- ✅ 聚合函数 (COUNT、SUM、AVG、MAX、MIN)
- ✅ 子查询
- ✅ 字符串和日期函数
- ✅ CASE语句

### 错误处理
- ✅ 语法错误检测
- ✅ 错误位置报告
- ✅ 错误消息清晰度

## 性能基准

测试程序会提供以下性能指标：
- 单次解析时间（微秒）
- 批量解析性能
- 不同SQL复杂度下的性能变化
- 与其他解析器的性能对比

## 注意事项

1. **ANTLR4依赖**: 确保系统已正确安装ANTLR4运行时库
2. **语法文件**: 确保SQL.g4语法文件存在且正确
3. **内存使用**: 复杂SQL语句可能消耗较多内存
4. **错误处理**: 测试程序会验证错误处理的准确性

## 扩展测试

如需添加新的测试用例，可以：
1. 在相应的测试函数中添加新的SQL语句
2. 创建新的测试函数来测试特定功能
3. 修改性能测试参数来适应不同的性能要求

## 故障排除

### 常见问题

1. **解析器创建失败**
   - 检查ANTLR4是否正确安装
   - 验证语法文件是否存在

2. **解析错误**
   - 检查SQL语法是否符合ANTLR4语法定义
   - 验证语法文件是否最新

3. **性能问题**
   - 检查系统资源使用情况
   - 验证编译优化设置

### 调试模式

可以通过修改测试程序来启用详细调试输出：
```cpp
// 在测试函数中添加详细输出
std::cout << "Debug: Parsing SQL: " << sql << std::endl;
```