# Examples

# 线程池测试程序
add_executable(thread_pool_test thread_pool_test.cpp)
target_link_libraries(thread_pool_test
    sealdb_common
    Threads::Threads
)

# 设置可执行文件输出目录
set_target_properties(thread_pool_test PROPERTIES
    RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/examples
)