# ANTLR4 Parser 模块

# 查找ANTLR4
find_package(ANTLR4 QUIET)

# 设置解析器选项
option(USE_ANTLR4_PARSER "Use ANTLR4 parser" ON)

set(ANTLR4_PARSER_SOURCES)
set(ANTLR4_PARSER_HEADERS)

# ANTLR4解析器
if(USE_ANTLR4_PARSER)
    # 检查ANTLR4是否可用
    if(NOT ANTLR4_FOUND)
        message(WARNING "ANTLR4 not found, creating interface library only.")
        set(USE_ANTLR4_PARSER OFF)
    endif()

    # 检查语法文件是否存在
    if(USE_ANTLR4_PARSER AND NOT EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/SQL.g4")
        message(WARNING "SQL.g4 not found, creating interface library only.")
        set(USE_ANTLR4_PARSER OFF)
    endif()

    # ANTLR4解析器实现
    if(USE_ANTLR4_PARSER)
        # ANTLR4语法文件
        set(ANTLR4_GRAMMAR_FILES
            SQL.g4
        )

        # 生成ANTLR4解析器
        antlr4_generate(${ANTLR4_GRAMMAR_FILES})

        list(APPEND ANTLR4_PARSER_SOURCES
            ${ANTLR4_GENERATED_SOURCES}
            antlr4_parser.cpp
            antlr4_lexer.cpp
        )

        list(APPEND ANTLR4_PARSER_HEADERS
            antlr4_parser.h
            antlr4_lexer.h
        )

        add_definitions(-DUSE_ANTLR4_PARSER)
    endif()
endif()

# 创建静态库
if(USE_ANTLR4_PARSER AND ANTLR4_PARSER_SOURCES)
    add_library(sealdb_sql_antlr4_parser STATIC ${ANTLR4_PARSER_SOURCES})

    # 设置包含目录
    target_include_directories(sealdb_sql_antlr4_parser PUBLIC
        ${CMAKE_SOURCE_DIR}/include
        ${CMAKE_CURRENT_SOURCE_DIR}
        ${CMAKE_CURRENT_BINARY_DIR}
    )

    # 链接依赖
    target_link_libraries(sealdb_sql_antlr4_parser
        sealdb_common
        antlr4-runtime
    )
else()
    # 创建空的接口库
    add_library(sealdb_sql_antlr4_parser INTERFACE)
    target_link_libraries(sealdb_sql_antlr4_parser INTERFACE sealdb_common)
endif()