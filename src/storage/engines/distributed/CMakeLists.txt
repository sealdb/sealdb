# Distributed Storage Engine 模块

set(DISTRIBUTED_SOURCES
    # 暂时为空，等待具体实现
)

set(DISTRIBUTED_HEADERS
    # 暂时为空，等待具体实现
)

# 创建空的接口库
add_library(sealdb_storage_distributed INTERFACE)
target_link_libraries(sealdb_storage_distributed INTERFACE sealdb_common)