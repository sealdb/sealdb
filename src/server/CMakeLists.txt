# Server 模块 - 对外服务（MySQL 协议、HTTP API）

set(SERVER_SOURCES
    mysql_server.cpp
    http_server.cpp
    connection_manager.cpp
    session.cpp
)

set(SERVER_HEADERS
    mysql_server.h
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
    sealdb_planner
    Threads::Threads
) 