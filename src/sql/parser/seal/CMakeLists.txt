# Seal Parser 模块 - 基于递归下降算法的SQL解析器

set(SEAL_PARSER_SOURCES
    seal_parser.cpp
    lexer.cpp
    parser.cpp
    ast.cpp
)

set(SEAL_PARSER_HEADERS
    seal_parser.h
    ast.h
    lexer.h
    parser.h
)

# 创建静态库
add_library(sealdb_sql_seal_parser STATIC ${SEAL_PARSER_SOURCES})

# 设置包含目录
target_include_directories(sealdb_sql_seal_parser PUBLIC
    ${CMAKE_SOURCE_DIR}/include
    ${CMAKE_CURRENT_SOURCE_DIR}
)

# 链接依赖
target_link_libraries(sealdb_sql_seal_parser
    sealdb_common
) 