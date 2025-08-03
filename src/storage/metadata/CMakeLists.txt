# Storage Metadata 模块

set(METADATA_SOURCES
    # 暂时为空，等待具体实现
)

set(METADATA_HEADERS
    # 暂时为空，等待具体实现
)

# 创建库
if(METADATA_SOURCES)
    add_library(sealdb_storage_metadata STATIC ${METADATA_SOURCES})

    # 设置包含目录
    target_include_directories(sealdb_storage_metadata PUBLIC
        ${CMAKE_SOURCE_DIR}/include
        ${CMAKE_CURRENT_SOURCE_DIR}
    )

    # 链接依赖
    target_link_libraries(sealdb_storage_metadata
        sealdb_common
    )
else()
    # 创建空的接口库
    add_library(sealdb_storage_metadata INTERFACE)
    target_link_libraries(sealdb_storage_metadata INTERFACE sealdb_common)
endif()