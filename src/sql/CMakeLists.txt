# SQL 模块 - SQL 解析与执行

set(SQL_SOURCES
    parser.cpp
    lexer.cpp
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