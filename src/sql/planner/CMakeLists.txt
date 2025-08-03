# Planner 模块 - 基于规则的优化 (RBO)

set(PLANNER_SOURCES
    planner.cpp
)

set(PLANNER_HEADERS
    planner.h
)

# 创建静态库
add_library(sealdb_sql_planner STATIC ${PLANNER_SOURCES})

# 设置包含目录
target_include_directories(sealdb_sql_planner PUBLIC
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# 链接依赖
target_link_libraries(sealdb_sql_planner
    sealdb_common
    sealdb_sql_parser
)