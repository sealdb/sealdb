# KV 模块 - TiKV 客户端

set(KV_SOURCES
    tikv_client.cpp
    tikv_connection.cpp
    tikv_transaction.cpp
    tikv_storage.cpp
)

set(KV_HEADERS
    tikv_client.h
    tikv_connection.h
    tikv_transaction.h
    tikv_storage.h
)

# 创建静态库
add_library(sealdb_kv STATIC ${KV_SOURCES})

# 设置包含目录
target_include_directories(sealdb_kv PUBLIC
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# 链接依赖
target_link_libraries(sealdb_kv
    sealdb_common
) 