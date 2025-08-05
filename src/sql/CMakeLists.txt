# SQL 模块 - SQL 解析与执行

# 添加子目录
add_subdirectory(parser)
add_subdirectory(executor)
add_subdirectory(planner)
add_subdirectory(optimizer)

set(SQL_SOURCES
    statement.cpp
)

set(SQL_HEADERS
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
    sealdb_storage
    sealdb_sql_parser
    sealdb_sql_executor
    sealdb_sql_planner
    sealdb_sql_optimizer
)