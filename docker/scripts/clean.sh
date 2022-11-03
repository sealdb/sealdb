#!/bin/bash

# Docker 清理脚本
# 用法: ./clean.sh [dev|prod|test|all]

set -e

ENVIRONMENT=${1:-all}

echo "清理 SealDB Docker 资源 (环境: $ENVIRONMENT)"

# 检查环境参数
if [[ ! "$ENVIRONMENT" =~ ^(dev|prod|test|all)$ ]]; then
    echo "错误: 环境参数必须是 dev, prod, test 或 all"
    echo "用法: $0 [dev|prod|test|all]"
    exit 1
fi

# 清理函数
clean_environment() {
    local env=$1
    echo "清理 $env 环境..."
    
    # 停止并删除容器
    cd docker/$env
    docker-compose down -v --remove-orphans
    cd ../..
    
    # 删除镜像
    docker rmi sealdb:$env 2>/dev/null || true
    
    echo "$env 环境清理完成"
}

# 执行清理
case $ENVIRONMENT in
    all)
        clean_environment "dev"
        clean_environment "prod"
        clean_environment "test"
        ;;
    *)
        clean_environment $ENVIRONMENT
        ;;
esac

echo "清理完成" 