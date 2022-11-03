cmake_minimum_required(VERSION 3.16)
project(SealDB VERSION 1.0.0 LANGUAGES CXX C)

# 设置 C++ 标准
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# 设置编译选项
set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -Wall -Wextra -O2")
set(CMAKE_CXX_FLAGS_DEBUG "${CMAKE_CXX_FLAGS_DEBUG} -g -O0 -DDEBUG")
set(CMAKE_CXX_FLAGS_RELEASE "${CMAKE_CXX_FLAGS_RELEASE} -O3 -DNDEBUG")

# 查找必要的包
find_package(PkgConfig)
find_package(Threads REQUIRED)

# 可选依赖 - 如果找不到也不会报错
find_package(Protobuf QUIET)
find_package(gRPC QUIET)
find_package(OpenSSL QUIET)
find_package(ZLIB QUIET)

# 设置包含目录
include_directories(
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_SOURCE_DIR}/src
)

# 添加子目录
add_subdirectory(src)
add_subdirectory(tests)
add_subdirectory(tools)
add_subdirectory(examples)

# 设置安装规则
install(TARGETS sealdb
    RUNTIME DESTINATION bin
    LIBRARY DESTINATION lib
    ARCHIVE DESTINATION lib
)

install(DIRECTORY include/ DESTINATION include/sealdb)