# SealDB Docker 配置

本目录包含 SealDB 项目的 Docker 相关配置文件，按照 Rust 项目的标准做法组织。

## 目录结构

```
docker/
├── dev/                 # 开发环境配置
│   ├── Dockerfile      # 开发环境 Dockerfile
│   └── docker-compose.yml
├── prod/               # 生产环境配置
│   ├── Dockerfile      # 生产环境 Dockerfile
│   └── docker-compose.yml
├── test/               # 测试环境配置
│   ├── Dockerfile      # 测试环境 Dockerfile
│   └── docker-compose.yml
├── scripts/            # Docker 脚本
│   ├── build.sh        # 构建脚本
│   ├── run.sh          # 运行脚本
│   └── clean.sh        # 清理脚本
├── .dockerignore       # Docker 忽略文件
└── README.md           # 本文件
```

## 环境说明

### 开发环境 (dev)
- 使用 debug 构建
- 简化的 TiKV 集群（1个 TiKV 节点）
- 详细的日志输出
- 适合开发和调试

### 生产环境 (prod)
- 使用 release 构建
- 完整的 TiKV 集群（3个 TiKV 节点）
- 包含监控服务（Prometheus + Grafana）
- 生产级别的配置和优化

### 测试环境 (test)
- 使用 release 构建
- 简化的 TiKV 集群
- 适合集成测试和性能测试

## 使用方法

### 构建镜像
```bash
# 构建生产环境镜像
./docker/scripts/build.sh prod

# 构建开发环境镜像
./docker/scripts/build.sh dev

# 构建测试环境镜像
./docker/scripts/build.sh test
```

### 运行容器
```bash
# 启动生产环境
./docker/scripts/run.sh prod up

# 启动开发环境
./docker/scripts/run.sh dev up

# 启动测试环境
./docker/scripts/run.sh test up

# 停止环境
./docker/scripts/run.sh prod down

# 重启环境
./docker/scripts/run.sh prod restart
```

### 清理资源
```bash
# 清理所有环境
./docker/scripts/clean.sh all

# 清理特定环境
./docker/scripts/clean.sh prod
```

## 端口映射

### 开发环境
- SealDB: 4000
- PD: 2379, 2380
- TiKV: 20160

### 生产环境
- SealDB: 4000
- PD: 2379, 2380
- TiKV: 20160, 20161, 20162
- Prometheus: 9090
- Grafana: 3000

### 测试环境
- SealDB: 4000
- PD: 2379, 2380
- TiKV: 20160

## 注意事项

1. 确保 Docker 和 Docker Compose 已安装
2. 生产环境需要更多资源，建议至少 4GB 内存
3. 首次启动可能需要较长时间下载镜像
4. 数据持久化存储在 Docker volumes 中
5. 监控数据默认保留 200 小时

## 故障排除

### 常见问题

1. **端口冲突**: 检查端口是否被占用
2. **内存不足**: 生产环境需要足够内存
3. **网络问题**: 检查 Docker 网络配置
4. **权限问题**: 确保有足够权限运行 Docker

### 日志查看
```bash
# 查看 SealDB 日志
docker logs sealdb-prod-app

# 查看 TiKV 日志
docker logs sealdb-prod-tikv1

# 查看所有容器日志
docker-compose -f docker/prod/docker-compose.yml logs
```