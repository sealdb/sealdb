# Examples 模块

# 线程池测试
add_executable(thread_pool_test thread_pool_test.cpp)

target_include_directories(thread_pool_test PRIVATE
    ${CMAKE_SOURCE_DIR}/include
)

target_link_libraries(thread_pool_test
    sealdb_common
)

# SQL解析器测试
add_executable(sql_parser_test sql_parser_test.cpp)

target_include_directories(sql_parser_test PRIVATE
    ${CMAKE_SOURCE_DIR}/include
)

target_link_libraries(sql_parser_test
    sealdb_common
    sealdb_sql
)

# 优化器测试
add_executable(optimizer_test optimizer_test.cpp)

target_include_directories(optimizer_test PRIVATE
    ${CMAKE_SOURCE_DIR}/include
)

target_link_libraries(optimizer_test
    sealdb_common
    sealdb_sql
    sealdb_planner
)

# Flex + Bison解析器测试
add_executable(flex_bison_test flex_bison_test.cpp)

target_include_directories(flex_bison_test PRIVATE
    ${CMAKE_SOURCE_DIR}/include
)

target_link_libraries(flex_bison_test
    sealdb_common
    sealdb_sql
)

# 设置可执行文件输出目录
set_target_properties(thread_pool_test PROPERTIES
    RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/examples
)