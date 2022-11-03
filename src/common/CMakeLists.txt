# Common 模块 - 通用工具和数据结构

set(COMMON_SOURCES
    error.cpp
    logger.cpp
    config.cpp
    utils.cpp
    buffer.cpp
    thread_pool.cpp
    connection.cpp
)

set(COMMON_HEADERS
    error.h
    logger.h
    config.h
    utils.h
    buffer.h
    thread_pool.h
    connection.h
)

# 创建静态库
add_library(sealdb_common STATIC ${COMMON_SOURCES})

# 设置包含目录
target_include_directories(sealdb_common PUBLIC
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# 链接线程库
target_link_libraries(sealdb_common
    Threads::Threads
)