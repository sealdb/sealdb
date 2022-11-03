#!/bin/bash

# Docker 构建脚本
# 用法: ./build.sh [dev|prod|test]

set -e

ENVIRONMENT=${1:-prod}

echo "构建 SealDB Docker 镜像 (环境: $ENVIRONMENT)"

# 检查环境参数
if [[ ! "$ENVIRONMENT" =~ ^(dev|prod|test)$ ]]; then
    echo "错误: 环境参数必须是 dev, prod 或 test"
    echo "用法: $0 [dev|prod|test]"
    exit 1
fi

# 构建镜像
echo "构建 $ENVIRONMENT 环境镜像..."
docker build -f docker/$ENVIRONMENT/Dockerfile -t sealdb:$ENVIRONMENT .

echo "构建完成: sealdb:$ENVIRONMENT" 