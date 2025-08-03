# Executor 模块 - SQL 执行器

set(EXECUTOR_SOURCES
    executor.cpp
)

set(EXECUTOR_HEADERS
    executor.h
    physical_operator.h
)

# 创建静态库
add_library(sealdb_sql_executor STATIC ${EXECUTOR_SOURCES})

# 设置包含目录
target_include_directories(sealdb_sql_executor PUBLIC
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# 链接依赖
target_link_libraries(sealdb_sql_executor
    sealdb_common
    sealdb_sql_parser
)