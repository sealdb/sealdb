# 添加所有子目录
add_subdirectory(common)
add_subdirectory(core)
add_subdirectory(kv)
add_subdirectory(sql)
add_subdirectory(planner)
add_subdirectory(server)

# 创建主可执行文件
add_executable(sealdb
    main.cpp
    sealdb.cpp
)

# 链接所有库
target_link_libraries(sealdb
    sealdb_common
    sealdb_core
    sealdb_kv
    sealdb_sql
    sealdb_planner
    sealdb_server
    Threads::Threads
)

# 设置可执行文件输出目录
set_target_properties(sealdb PROPERTIES
    RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/bin
)