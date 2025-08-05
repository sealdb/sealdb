# Heap Storage Engine 模块

set(HEAP_SOURCES
    # 暂时为空，等待具体实现
)

set(HEAP_HEADERS
    # 暂时为空，等待具体实现
)

# 创建空的接口库
add_library(sealdb_storage_heap INTERFACE)
target_link_libraries(sealdb_storage_heap INTERFACE sealdb_common)