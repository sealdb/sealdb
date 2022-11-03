#!/bin/bash

# Docker 运行脚本
# 用法: ./run.sh [dev|prod|test] [up|down|restart]

set -e

ENVIRONMENT=${1:-prod}
ACTION=${2:-up}

echo "运行 SealDB Docker 容器 (环境: $ENVIRONMENT, 操作: $ACTION)"

# 检查环境参数
if [[ ! "$ENVIRONMENT" =~ ^(dev|prod|test)$ ]]; then
    echo "错误: 环境参数必须是 dev, prod 或 test"
    echo "用法: $0 [dev|prod|test] [up|down|restart]"
    exit 1
fi

# 检查操作参数
if [[ ! "$ACTION" =~ ^(up|down|restart)$ ]]; then
    echo "错误: 操作参数必须是 up, down 或 restart"
    echo "用法: $0 [dev|prod|test] [up|down|restart]"
    exit 1
fi

# 切换到对应环境目录
cd docker/$ENVIRONMENT

# 执行操作
case $ACTION in
    up)
        echo "启动 $ENVIRONMENT 环境..."
        docker-compose up -d
        ;;
    down)
        echo "停止 $ENVIRONMENT 环境..."
        docker-compose down
        ;;
    restart)
        echo "重启 $ENVIRONMENT 环境..."
        docker-compose restart
        ;;
esac

echo "操作完成: $ACTION $ENVIRONMENT" 