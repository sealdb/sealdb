# Storage Engines 模块

# 添加子目录
add_subdirectory(kv)
add_subdirectory(heap)
add_subdirectory(columnar)
add_subdirectory(distributed)

set(ENGINES_SOURCES
    # 暂时为空，等待具体实现
)

set(ENGINES_HEADERS
    # 暂时为空，等待具体实现
)

# 创建库
if(ENGINES_SOURCES)
    add_library(sealdb_storage_engines STATIC ${ENGINES_SOURCES})

    # 设置包含目录
    target_include_directories(sealdb_storage_engines PUBLIC
        ${CMAKE_SOURCE_DIR}/include
        ${CMAKE_CURRENT_SOURCE_DIR}
    )

    # 链接依赖
    target_link_libraries(sealdb_storage_engines
        sealdb_common
    )
else()
    # 创建空的接口库
    add_library(sealdb_storage_engines INTERFACE)
    target_link_libraries(sealdb_storage_engines INTERFACE
        sealdb_common
        sealdb_kv
        sealdb_storage_heap
        sealdb_storage_columnar
        sealdb_storage_distributed
    )
endif()