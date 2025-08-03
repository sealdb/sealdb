#!/bin/bash

# SealDB SQL解析器构建脚本

set -e

echo "=== SealDB SQL解析器构建脚本 ==="

# 检查依赖
echo "检查依赖..."

# 检查CMake
if ! command -v cmake &> /dev/null; then
    echo "错误: 未找到CMake，请先安装CMake"
    exit 1
fi

# 检查ANTLR4
if ! command -v antlr4 &> /dev/null; then
    echo "警告: 未找到ANTLR4工具，将使用PostgreSQL解析器"
    USE_ANTLR4=OFF
else
    echo "找到ANTLR4工具"
    USE_ANTLR4=ON
fi

# 检查Flex和Bison
if ! command -v flex &> /dev/null || ! command -v bison &> /dev/null; then
    echo "警告: 未找到Flex或Bison，PostgreSQL解析器将不可用"
    USE_POSTGRESQL=OFF
else
    echo "找到Flex和Bison"
    USE_POSTGRESQL=ON
fi

# 创建构建目录
echo "创建构建目录..."
mkdir -p build
cd build

# 配置CMake
echo "配置CMake..."
cmake .. \
    -DUSE_ANTLR4_PARSER=${USE_ANTLR4} \
    -DUSE_POSTGRESQL_PARSER=${USE_POSTGRESQL} \
    -DCMAKE_BUILD_TYPE=Debug \
    -DCMAKE_MODULE_PATH="${PWD}/cmake"

# 编译
echo "编译项目..."
make -j$(nproc)

echo "=== 构建完成 ==="
echo "可执行文件位置: build/bin/sealdb"
echo "测试文件位置: build/bin/sql_parser_test"

# 运行测试
if [ -f "bin/sql_parser_test" ]; then
    echo "运行解析器测试..."
    ./bin/sql_parser_test
fi

echo "=== 构建脚本完成 ===" 