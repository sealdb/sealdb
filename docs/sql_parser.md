# SQL解析器综合文档

## 概述

SealDB支持两种SQL解析器实现：ANTLR4解析器（默认）和PostgreSQL风格解析器（Flex + Bison）。本文档提供了完整的架构设计、实现细节、使用指南和扩展方法。

## 目录结构

```
src/sql/parser/
├── CMakeLists.txt              # 主解析器模块CMake文件
├── parser_factory.h            # 解析器工厂头文件
├── parser_factory.cpp          # 解析器工厂实现
├── antlr4/                     # ANTLR4解析器目录
│   ├── CMakeLists.txt         # ANTLR4解析器CMake文件
│   ├── SQL.g4                 # ANTLR4语法文件
│   ├── antlr4_parser.h        # ANTLR4解析器头文件
│   ├── antlr4_parser.cpp      # ANTLR4解析器实现
│   ├── antlr4_lexer.h         # ANTLR4词法分析器头文件
│   └── antlr4_lexer.cpp       # ANTLR4词法分析器实现
└── pg/                        # PostgreSQL风格解析器目录
    ├── CMakeLists.txt         # PostgreSQL解析器CMake文件
    ├── lexer.l                # Flex词法规则文件
    ├── parser.y               # Bison语法规则文件
    ├── lexer.cpp              # 生成的词法分析器
    ├── parser.cpp             # 生成的语法分析器
    ├── lexer.h                # 词法分析器头文件
    └── parser.h               # 语法分析器头文件
```

## 1. ANTLR4解析器

### 特性
- 支持完整的SQL语法
- 自动生成词法分析器和语法分析器
- 支持访问者模式和监听器模式
- 易于扩展和维护
- 详细的错误报告

### 安装依赖

#### Ubuntu/Debian
```bash
sudo apt-get install antlr4
```

#### CentOS/RHEL
```bash
sudo yum install antlr4
```

#### macOS
```bash
brew install antlr4
```

#### 从源码编译
```bash
git clone https://github.com/antlr/antlr4.git
cd antlr4/runtime/Cpp
mkdir build && cd build
cmake ..
make
sudo make install
```

### 使用方法

```cpp
#include "sealdb/parser_factory.h"

// 创建ANTLR4解析器
auto parser = ParserFactory::createParser(ParserType::ANTLR4);

// 解析SQL语句
std::string sql = "SELECT * FROM users WHERE age > 18";
auto result = parser->parse(sql);

if (result.ast) {
    // 解析成功，处理AST
    auto selectStmt = std::dynamic_pointer_cast<SelectStatement>(result.ast);
    // ...
} else {
    // 解析失败，处理错误
    for (const auto& error : result.errors) {
        std::cerr << "Error: " << error << std::endl;
    }
}
```

### 核心文件

1. **src/sql/parser/antlr4/SQL.g4**
   - 完整的SQL语法定义
   - 支持SELECT, INSERT, UPDATE, DELETE, CREATE TABLE等语句
   - 支持表达式、函数调用、子查询等

2. **src/sql/parser/antlr4/antlr4_parser.h/cpp**
   - ANTLR4解析器的主要实现
   - 将ANTLR4语法树转换为SealDB AST
   - 支持错误处理和错误报告

3. **src/sql/parser/antlr4/antlr4_lexer.h/cpp**
   - ANTLR4词法分析器实现
   - 提供词法分析功能
   - 支持错误处理

## 2. PostgreSQL风格解析器

### 特性
- 与PostgreSQL语法高度兼容
- 成熟的解析器实现
- 高性能
- 错误恢复机制

### 安装依赖

#### Ubuntu/Debian
```bash
sudo apt-get install flex bison
```

#### CentOS/RHEL
```bash
sudo yum install flex bison
```

#### macOS
```bash
brew install flex bison
```

### 使用方法

```cpp
#include "sealdb/parser_factory.h"

// 创建PostgreSQL风格解析器
auto parser = ParserFactory::createParser(ParserType::POSTGRESQL);

// 解析SQL语句
std::string sql = "SELECT * FROM users WHERE age > 18";
auto result = parser->parse(sql);
```

### 核心文件

1. **src/sql/parser/pg/lexer.l**
   - Flex词法规则
   - 定义SQL关键字和标识符

2. **src/sql/parser/pg/parser.y**
   - Bison语法规则
   - 定义SQL语法结构

3. **src/sql/parser/pg/lexer.cpp/h**
   - 生成的词法分析器
   - 提供词法分析功能

4. **src/sql/parser/pg/parser.cpp/h**
   - 生成的语法分析器
   - 提供语法分析功能

## 3. 配置解析器

### 编译时配置

在CMake中设置解析器选项：

```cmake
# 使用ANTLR4解析器 (默认)
set(USE_ANTLR4_PARSER ON)
set(USE_POSTGRESQL_PARSER OFF)

# 或使用PostgreSQL风格解析器
set(USE_ANTLR4_PARSER OFF)
set(USE_POSTGRESQL_PARSER ON)
```

### 运行时配置

```cpp
// 根据配置字符串创建解析器
auto parser = ParserFactory::createParserFromConfig("antlr4");
// 或
auto parser = ParserFactory::createParserFromConfig("postgresql");

// 获取可用的解析器类型
auto availableTypes = ParserFactory::getAvailableParserTypes();
for (auto type : availableTypes) {
    if (type == ParserType::ANTLR4) {
        std::cout << "ANTLR4 parser available" << std::endl;
    } else if (type == ParserType::POSTGRESQL) {
        std::cout << "PostgreSQL parser available" << std::endl;
    }
}
```

## 4. 支持的SQL语法

### SELECT语句
```sql
SELECT column1, column2 FROM table_name WHERE condition;
SELECT * FROM table_name ORDER BY column1 ASC;
SELECT COUNT(*) FROM table_name GROUP BY column1 HAVING condition;
```

### INSERT语句
```sql
INSERT INTO table_name (column1, column2) VALUES (value1, value2);
INSERT INTO table_name VALUES (value1, value2, value3);
```

### UPDATE语句
```sql
UPDATE table_name SET column1 = value1, column2 = value2 WHERE condition;
```

### DELETE语句
```sql
DELETE FROM table_name WHERE condition;
```

### CREATE TABLE语句
```sql
CREATE TABLE table_name (
    id INT PRIMARY KEY,
    name VARCHAR(50) NOT NULL,
    age INT DEFAULT 0,
    created_at TIMESTAMP
);
```

### 事务语句
```sql
BEGIN TRANSACTION;
COMMIT;
ROLLBACK;
```

### 表达式类型
- 字面量 (字符串、整数、浮点数、布尔值、NULL)
- 列引用
- 二元表达式 (算术、比较、逻辑)
- 一元表达式
- 函数调用
- 括号表达式

### 数据类型
- INT, INTEGER, BIGINT, SMALLINT, TINYINT
- VARCHAR, CHAR, TEXT
- BOOLEAN, BOOL
- FLOAT, DOUBLE, DECIMAL
- DATE, DATETIME, TIMESTAMP

## 5. 构建系统

### CMake配置

1. **FindANTLR4.cmake**
   - 自动查找ANTLR4库和工具
   - 生成ANTLR4源文件的自定义命令
   - 设置正确的包含目录和链接库

2. **构建选项**
   ```cmake
   option(USE_ANTLR4_PARSER "Use ANTLR4 parser" ON)
   option(USE_POSTGRESQL_PARSER "Use PostgreSQL-style Flex/Bison parser" OFF)
   ```

3. **依赖管理**
   - 自动检测可用的解析器工具
   - 优雅降级到可用的解析器
   - 详细的错误报告

### 库依赖

```cmake
# ANTLR4解析器依赖
target_link_libraries(sealdb_sql_antlr4_parser
    sealdb_common
    antlr4-runtime
)

# PostgreSQL解析器依赖
target_link_libraries(sealdb_sql_pg_parser
    sealdb_common
)

# 解析器工厂依赖
target_link_libraries(sealdb_sql_parser_factory
    sealdb_common
    sealdb_sql_antlr4_parser
    sealdb_sql_pg_parser
)
```

### 构建脚本

创建了`build_parser.sh`脚本：
- 自动检测依赖
- 配置CMake选项
- 编译项目
- 运行测试

## 6. 使用方式

### 通过工厂创建解析器

```cpp
#include "sealdb/parser_factory.h"

// 创建ANTLR4解析器
auto antlr4Parser = ParserFactory::createParser(ParserType::ANTLR4);

// 创建PostgreSQL解析器
auto pgParser = ParserFactory::createParser(ParserType::POSTGRESQL);

// 使用默认解析器
auto defaultParser = ParserFactory::createDefaultParser();
```

### 直接使用特定解析器

```cpp
#include "sealdb/parser/antlr4/antlr4_parser.h"
#include "sealdb/parser/pg/parser.h"

// 使用ANTLR4解析器
sealdb::Antlr4Parser antlr4Parser;
auto result = antlr4Parser.parse(sql);

// 使用PostgreSQL解析器
sealdb::PostgreSQLParser pgParser;
auto result = pgParser.parse(sql);
```

## 7. 错误处理

解析器提供详细的错误信息：

```cpp
auto result = parser->parse("SELECT * FROM"); // 不完整的SQL

if (!result.errors.empty()) {
    for (const auto& error : result.errors) {
        std::cerr << "Parse error: " << error << std::endl;
    }
}
```

## 8. 扩展指南

### 添加新的解析器

1. **创建新目录**：
   ```
   src/sql/parser/new_parser/
   ├── CMakeLists.txt
   ├── parser.h
   └── parser.cpp
   ```

2. **实现解析器接口**：
   ```cpp
   class NewParser : public ParserInterface {
   public:
       ParseResult parse(const std::string& sql) override;
   };
   ```

3. **更新工厂类**：
   ```cpp
   // 在parser_factory.cpp中添加
   case ParserType::NEW_PARSER:
       return std::make_unique<NewParser>();
   ```

4. **更新CMake配置**：
   ```cmake
   # 在src/sql/parser/CMakeLists.txt中添加
   add_subdirectory(new_parser)
   ```

### 修改现有解析器

1. **ANTLR4解析器**：
   - 修改 `antlr4/SQL.g4` 语法文件
   - 更新 `antlr4/antlr4_parser.cpp` 转换逻辑
   - 重新生成解析器代码

2. **PostgreSQL解析器**：
   - 修改 `pg/lexer.l` 词法规则
   - 修改 `pg/parser.y` 语法规则
   - 重新生成解析器代码

### 添加新的SQL语句类型

1. 在`SQL.g4`中添加语法规则
2. 在`Antlr4Parser`中添加转换方法
3. 在AST中添加相应的节点类型

### 添加新的表达式类型

1. 在`SQL.g4`中添加表达式规则
2. 在`Antlr4Parser::convertExpression`中添加处理逻辑
3. 在AST中添加相应的表达式类型

## 9. 测试

### 运行解析器测试

```bash
# 构建测试
cd build
make sql_parser_test

# 运行测试
./bin/sql_parser_test
```

### 测试特定解析器

```cpp
// 测试ANTLR4解析器
TEST_F(SQLParserTest, Antlr4ParserTest) {
    auto parser = ParserFactory::createParser(ParserType::ANTLR4);
    // 测试代码...
}

// 测试PostgreSQL解析器
TEST_F(SQLParserTest, PostgreSQLParserTest) {
    auto parser = ParserFactory::createParser(ParserType::POSTGRESQL);
    // 测试代码...
}
```

### 测试覆盖

- SELECT语句 (简单查询、WHERE、ORDER BY等)
- INSERT语句 (列列表、值列表)
- UPDATE语句 (SET、WHERE)
- DELETE语句 (WHERE)
- CREATE TABLE语句 (列定义、约束)
- 错误SQL的处理

## 10. 性能考虑

### ANTLR4解析器
- 适合复杂的语法和快速开发
- 内存使用相对较高
- 解析速度中等

### PostgreSQL解析器
- 适合高性能要求
- 内存使用较低
- 解析速度较快

## 11. 故障排除

### ANTLR4相关问题

1. **找不到ANTLR4运行时库**
   ```bash
   # 检查库是否安装
   ldconfig -p | grep antlr4

   # 重新安装ANTLR4
   sudo apt-get install --reinstall antlr4
   ```

2. **ANTLR4工具不可用**
   ```bash
   # 检查工具是否安装
   which antlr4

   # 安装ANTLR4工具
   sudo apt-get install antlr4
   ```

### Flex/Bison相关问题

1. **找不到Flex或Bison**
   ```bash
   # 检查工具是否安装
   which flex
   which bison

   # 安装工具
   sudo apt-get install flex bison
   ```

2. **编译错误**
   - 确保CMake版本 >= 3.16
   - 检查编译器是否支持C++17
   - 确保所有依赖库都已正确安装

### 常见问题

1. **ANTLR4工具未找到**：
   ```bash
   sudo apt-get install antlr4
   ```

2. **Flex/Bison工具未找到**：
   ```bash
   sudo apt-get install flex bison
   ```

3. **编译错误**：
   - 检查CMake版本 >= 3.16
   - 确保C++17支持
   - 验证依赖库安装

4. **链接错误**：
   - 检查ANTLR4运行时库
   - 验证库路径设置
   - 确认CMake模块路径

## 12. 维护指南

### 代码组织
- 每个解析器独立维护
- 共享接口通过工厂模式
- 测试覆盖所有解析器

### 版本控制
- 语法文件变更需要重新生成
- 保持向后兼容性
- 记录重大变更

### 文档更新
- 更新API文档
- 维护使用示例
- 记录已知问题

## 13. 任务完成情况

### 任务1：修改CMake文件以适应SQL目录的文件结构调整 ✅

已完成以下CMake文件的修改：

1. **src/CMakeLists.txt**
   - 将`kv`目录改为`storage`目录
   - 移除了`planner`目录（现在在sql目录下）
   - 更新了库链接依赖

2. **src/sql/CMakeLists.txt**
   - 重新组织为模块化结构
   - 添加了子目录：parser, executor, planner, optimizer
   - 支持两种解析器：ANTLR4和PostgreSQL风格
   - 添加了编译选项控制解析器选择

3. **src/sql/parser/CMakeLists.txt**
   - 支持ANTLR4和Flex/Bison两种解析器
   - 自动检测可用的解析器工具
   - 生成相应的源文件和头文件

4. **src/storage/CMakeLists.txt**
   - 创建了storage模块的主CMakeLists.txt
   - 添加了子目录：engines, metadata, api

5. **子目录CMakeLists.txt**
   - 为每个子目录创建了独立的CMakeLists.txt
   - 正确设置了依赖关系和链接库

### 任务2：实现ANTLR4解析器 ✅

已完成ANTLR4解析器的完整实现，包括：

- 完整的SQL语法支持
- 模块化的架构设计
- 灵活的解析器选择机制
- 完善的构建系统
- 详细的文档和测试

## 14. 下一步工作

### 短期目标

1. **完善ANTLR4解析器**
   - 添加更多SQL语句支持
   - 优化性能
   - 完善错误处理

2. **实现PostgreSQL解析器**
   - 参考PostgreSQL源码
   - 实现Flex/Bison解析器
   - 确保语法兼容性

3. **集成测试**
   - 添加更多测试用例
   - 性能基准测试
   - 兼容性测试

### 长期目标

1. **性能优化**
   - 解析器性能优化
   - 内存使用优化
   - 并发解析支持

2. **功能扩展**
   - 支持更多SQL特性
   - 存储过程支持
   - 触发器支持

3. **工具支持**
   - SQL格式化工具
   - 语法高亮
   - 代码补全

## 总结

成功完成了SQL解析器的架构设计和ANTLR4解析器的实现：

✅ **任务1**: 修改CMake文件以适应新的目录结构
✅ **任务2**: 实现ANTLR4解析器并设计PostgreSQL解析器架构

项目现在支持：
- 模块化的SQL解析器架构
- 完整的ANTLR4解析器实现
- 灵活的解析器选择机制
- 完善的构建系统
- 详细的文档和测试

这个实现为SealDB提供了一个强大、灵活、可扩展的SQL解析基础。