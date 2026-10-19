#!/bin/bash

# SealDB 集成测试运行脚本

set -e

echo "=== SealDB 集成测试 ==="

# 检查是否在正确的目录
if [ ! -f "Cargo.toml" ]; then
    echo "错误: 请在项目根目录运行此脚本"
    exit 1
fi

echo "1. 运行单元测试..."
cargo test --lib

echo "2. 运行集成测试..."
cargo test -p sealdb-integration-tests

echo "3. 运行回归测试..."
cd tests/regression/test_framework
cargo test
cd ../../..

echo "=== 所有测试完成 ==="