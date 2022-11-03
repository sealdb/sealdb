# Planner 模块 - 查询优化与执行计划

set(PLANNER_SOURCES
    optimizer.cpp
    planner.cpp
    cost_estimator.cpp
    index_selector.cpp
)

set(PLANNER_HEADERS
    optimizer.h
    planner.h
    cost_estimator.h
    index_selector.h
)

# 创建静态库
add_library(sealdb_planner STATIC ${PLANNER_SOURCES})

# 设置包含目录
target_include_directories(sealdb_planner PUBLIC
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# 链接依赖
target_link_libraries(sealdb_planner
    sealdb_common
    sealdb_sql
) 