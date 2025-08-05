# Columnar Storage Engine 模块

set(COLUMNAR_SOURCES
    # 暂时为空，等待具体实现
)

set(COLUMNAR_HEADERS
    # 暂时为空，等待具体实现
)

# 创建空的接口库
add_library(sealdb_storage_columnar INTERFACE)
target_link_libraries(sealdb_storage_columnar INTERFACE sealdb_common)