# Optimizer 模块 - 基于成本的优化 (CBO)

set(OPTIMIZER_SOURCES
    optimizer.cpp
    cost_estimator.cpp
    index_selector.cpp
    statistics_manager.cpp
)

set(OPTIMIZER_HEADERS
    optimizer.h
    cost_estimator.h
    index_selector.h
    statistics_manager.h
)

# 创建静态库
add_library(sealdb_sql_optimizer STATIC ${OPTIMIZER_SOURCES})

# 设置包含目录
target_include_directories(sealdb_sql_optimizer PUBLIC
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# 链接依赖
target_link_libraries(sealdb_sql_optimizer
    sealdb_common
    sealdb_sql_parser
    sealdb_sql_planner
)