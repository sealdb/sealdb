# PostgreSQL Parser 模块

# 查找flex和bison
find_package(FLEX QUIET)
find_package(BISON QUIET)

# 设置解析器选项
option(USE_POSTGRESQL_PARSER "Use PostgreSQL-style Flex/Bison parser" OFF)

set(PG_PARSER_SOURCES)
set(PG_PARSER_HEADERS)

# PostgreSQL风格解析器 (Flex + Bison)
if(USE_POSTGRESQL_PARSER)
    # 检查Flex和Bison是否可用
    if(NOT FLEX_FOUND)
        message(FATAL_ERROR "Flex not found but USE_POSTGRESQL_PARSER is enabled. Please install flex or disable USE_POSTGRESQL_PARSER.")
    endif()

    if(NOT BISON_FOUND)
        message(FATAL_ERROR "Bison not found but USE_POSTGRESQL_PARSER is enabled. Please install bison or disable USE_POSTGRESQL_PARSER.")
    endif()

    # 检查源文件是否存在
    if(NOT EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/lexer.l")
        message(FATAL_ERROR "lexer.l not found but USE_POSTGRESQL_PARSER is enabled.")
    endif()

    if(NOT EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/parser.y")
        message(FATAL_ERROR "parser.y not found but USE_POSTGRESQL_PARSER is enabled.")
    endif()

    # 生成词法分析器
    FLEX_TARGET(lexer
        lexer.l
        ${CMAKE_CURRENT_BINARY_DIR}/lexer.cpp
        VERBOSE ${CMAKE_CURRENT_SOURCE_DIR}/lexer.l
    )

    # 生成语法分析器
    BISON_TARGET(parser
        parser.y
        ${CMAKE_CURRENT_BINARY_DIR}/parser.cpp
        VERBOSE ${CMAKE_CURRENT_SOURCE_DIR}/parser.y
    )

    # 添加生成的源文件
    ADD_FLEX_BISON_DEPENDENCY(lexer parser)

    list(APPEND PG_PARSER_SOURCES
        ${CMAKE_CURRENT_BINARY_DIR}/lexer.cpp
        ${CMAKE_CURRENT_BINARY_DIR}/parser.cpp
    )

    list(APPEND PG_PARSER_HEADERS
        lexer.h
        parser.h
    )

    add_definitions(-DUSE_POSTGRESQL_PARSER)
endif()

# 创建静态库
if(USE_POSTGRESQL_PARSER)
    add_library(sealdb_sql_pg_parser STATIC ${PG_PARSER_SOURCES})

    # 设置包含目录
    target_include_directories(sealdb_sql_pg_parser PUBLIC
        ${CMAKE_SOURCE_DIR}/include
        ${CMAKE_CURRENT_SOURCE_DIR}
        ${CMAKE_CURRENT_BINARY_DIR}
    )

    # 链接依赖
    target_link_libraries(sealdb_sql_pg_parser
        sealdb_common
    )
else()
    # 创建空的接口库
    add_library(sealdb_sql_pg_parser INTERFACE)
    target_link_libraries(sealdb_sql_pg_parser INTERFACE sealdb_common)
endif()