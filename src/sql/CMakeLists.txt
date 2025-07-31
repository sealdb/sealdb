# SQL 模块 - SQL 解析与执行

# 查找flex和bison
find_package(FLEX REQUIRED)
find_package(BISON REQUIRED)

# 生成词法分析器
FLEX_TARGET(lexer
    lexer.l
    ${CMAKE_CURRENT_BINARY_DIR}/lexer.cpp
    VERBOSE ${CMAKE_CURRENT_SOURCE_DIR}/lexer.l
)

# 生成语法分析器
BISON_TARGET(parser
    parser.y
    ${CMAKE_CURRENT_BINARY_DIR}/parser.cpp
    VERBOSE ${CMAKE_CURRENT_SOURCE_DIR}/parser.y
)

# 添加生成的源文件
ADD_FLEX_BISON_DEPENDENCY(lexer parser)

set(SQL_SOURCES
    ${CMAKE_CURRENT_BINARY_DIR}/lexer.cpp
    ${CMAKE_CURRENT_BINARY_DIR}/parser.cpp
    ast.cpp
    executor.cpp
    statement.cpp
)

set(SQL_HEADERS
    parser.h
    lexer.h
    ast.h
    executor.h
    statement.h
)

# 创建静态库
add_library(sealdb_sql STATIC ${SQL_SOURCES})

# 设置包含目录
target_include_directories(sealdb_sql PUBLIC
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# 链接依赖
target_link_libraries(sealdb_sql
    sealdb_common
    sealdb_kv
)