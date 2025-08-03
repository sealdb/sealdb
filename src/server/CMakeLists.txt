# Server 模块 - 对外服务（多协议支持）

set(SERVER_SOURCES
    mysql_server.cpp
    http_server.cpp
    connection_manager.cpp
    session.cpp
)

set(SERVER_HEADERS
    protocol.h
    mysql_protocol.h
    http_server.h
    connection_manager.h
    session.h
)

# 创建静态库
add_library(sealdb_server STATIC ${SERVER_SOURCES})

# 设置包含目录
target_include_directories(sealdb_server PUBLIC
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# 链接依赖
target_link_libraries(sealdb_server
    sealdb_common
    sealdb_sql
    Threads::Threads
)