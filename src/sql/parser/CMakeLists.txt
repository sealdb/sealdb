# Parser 模块 - SQL 解析器

# 添加子目录
add_subdirectory(antlr4)
add_subdirectory(pg)

# 查找flex和bison (用于PostgreSQL风格的解析器)
find_package(FLEX QUIET)
find_package(BISON QUIET)

# 查找ANTLR4 (用于ANTLR4解析器)
find_package(ANTLR4 QUIET)

# 设置解析器选项
option(USE_POSTGRESQL_PARSER "Use PostgreSQL-style Flex/Bison parser" OFF)
option(USE_ANTLR4_PARSER "Use ANTLR4 parser" ON)

# 默认使用ANTLR4解析器
if(NOT USE_POSTGRESQL_PARSER AND NOT USE_ANTLR4_PARSER)
    set(USE_ANTLR4_PARSER ON)
endif()

# 创建解析器工厂库
set(PARSER_FACTORY_SOURCES
    parser_factory.cpp
)

set(PARSER_FACTORY_HEADERS
    parser_factory.h
)

# 创建解析器工厂静态库
add_library(sealdb_sql_parser_factory STATIC ${PARSER_FACTORY_SOURCES})

# 设置包含目录
target_include_directories(sealdb_sql_parser_factory PUBLIC
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# 链接依赖
target_link_libraries(sealdb_sql_parser_factory
    sealdb_common
    sealdb_sql_antlr4_parser
    sealdb_sql_pg_parser
)