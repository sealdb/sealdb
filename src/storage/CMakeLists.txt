# Storage 模块 - 存储引擎

# 添加子目录
add_subdirectory(engines)
add_subdirectory(metadata)
add_subdirectory(api)

# 创建存储模块主库
add_library(sealdb_storage INTERFACE)

# 设置包含目录
target_include_directories(sealdb_storage INTERFACE
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# 链接依赖
target_link_libraries(sealdb_storage INTERFACE
    sealdb_common
    sealdb_storage_engines
    sealdb_storage_metadata
    sealdb_storage_api
) 