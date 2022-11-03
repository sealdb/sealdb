# SealDB 命令行使用指南

本文档提供了 SealDB 命令行工具的详细使用示例。

## 快速开始

### 1. 基本启动

```bash
# 使用默认配置文件启动
./target/debug/sealdb

# 使用指定配置文件启动
./target/debug/sealdb -c/--config config/sealdb.yaml
```

### 2. 查看帮助和版本

```bash
# 查看帮助信息
./target/debug/sealdb -h/--help

# 查看版本信息
./target/debug/sealdb -V/--version

# 查看详细版本信息
./target/debug/sealdb --verbose-version
```

## 常用场景

### 开发环境

```bash
# 开发环境 - 本地调试
./target/debug/sealdb \
  -H/--host 127.0.0.1 \
  -P/--port 4000 \
  -L/--log-level debug \
  -O/--console-log

# 开发环境 - 指定配置文件
./target/debug/sealdb \
  -c/--config config/sealdb.toml \
  -H/--host 127.0.0.1 \
  -P/--port 8080 \
  -L/--log-level debug
```

### 测试环境

```bash
# 测试环境 - 使用测试配置
./target/debug/sealdb \
  --config config/sealdb.yaml \
  --host 0.0.0.0 \
  --port 4000 \
  --log-level info \
  --tikv-pd-endpoints 192.168.1.100:2379,192.168.1.101:2379
```

### 生产环境

```bash
# 生产环境 - 守护进程模式
./target/debug/sealdb \
  --config config/sealdb.toml \
  --host 0.0.0.0 \
  --port 4000 \
  --log-level warn \
  --log-file /var/log/sealdb.log \
  --daemon \
  --pid-file /var/run/sealdb.pid

# 生产环境 - 高可用配置
./target/debug/sealdb \
  --config config/sealdb.json \
  --host 0.0.0.0 \
  --port 4000 \
  --max-connections 5000 \
  --log-level info \
  --log-file /var/log/sealdb.log \
  --tikv-pd-endpoints 192.168.1.100:2379,192.168.1.101:2379,192.168.1.102:2379 \
  --tikv-connect-timeout 10000 \
  --tikv-request-timeout 15000 \
  --max-query-time 60000 \
  --max-memory-usage 2147483648 \
  --daemon
```

## 配置管理

### 生成配置文件

```bash
# 生成所有格式的默认配置文件
./target/debug/sealdb --generate-config
```

### 查看配置

```bash
# 查看当前配置
./target/debug/sealdb --config config/sealdb.toml --show-config

# 查看命令行覆盖后的配置
./target/debug/sealdb \
  --config config/sealdb.toml \
  --host 127.0.0.1 \
  --port 8080 \
  --show-config
```

## 参数组合示例

### 服务器配置

```bash
# 基本服务器配置
./target/debug/sealdb --host 0.0.0.0 --port 4000

# 高并发配置
./target/debug/sealdb \
  --host 0.0.0.0 \
  --port 4000 \
  --max-connections 10000
```

### 日志配置

```bash
# 开发调试
./target/debug/sealdb --log-level debug --console-log

# 生产环境
./target/debug/sealdb \
  --log-level info \
  --log-file /var/log/sealdb.log

# 错误监控
./target/debug/sealdb \
  --log-level error \
  --log-file /var/log/sealdb-error.log
```

### 存储配置

```bash
# 单节点 TiKV
./target/debug/sealdb \
  --tikv-pd-endpoints 127.0.0.1:2379

# 多节点 TiKV 集群
./target/debug/sealdb \
  --tikv-pd-endpoints 192.168.1.100:2379,192.168.1.101:2379,192.168.1.102:2379

# 自定义超时配置
./target/debug/sealdb \
  --tikv-pd-endpoints 192.168.1.100:2379,192.168.1.101:2379 \
  --tikv-connect-timeout 15000 \
  --tikv-request-timeout 20000
```

### SQL 查询配置

```bash
# 长时间查询
./target/debug/sealdb \
  --max-query-time 300000 \
  --max-memory-usage 4294967296

# 快速查询
./target/debug/sealdb \
  --max-query-time 10000 \
  --max-memory-usage 1073741824
```

## 错误处理

### 常见错误及解决方案

1. **配置文件不存在**
   ```bash
   Error: Failed to read config file: config.toml
   ```
   解决方案：使用 `--generate-config` 生成配置文件

2. **无效的日志级别**
   ```bash
   Error: 无效的日志级别: invalid
   ```
   解决方案：使用有效的日志级别（debug, info, warn, error）

3. **端口被占用**
   ```bash
   Error: 端口不能为 0
   ```
   解决方案：指定有效的端口号

4. **TiKV 连接失败**
   ```bash
   Error: 至少需要指定一个 TiKV PD 端点
   ```
   解决方案：指定正确的 TiKV PD 端点

## 性能调优

### 内存配置

```bash
# 小内存环境（1GB）
./target/debug/sealdb \
  --max-memory-usage 1073741824

# 大内存环境（8GB）
./target/debug/sealdb \
  --max-memory-usage 8589934592
```

### 连接数配置

```bash
# 低并发环境
./target/debug/sealdb \
  --max-connections 100

# 高并发环境
./target/debug/sealdb \
  --max-connections 10000
```

### 查询超时配置

```bash
# 快速查询环境
./target/debug/sealdb \
  --max-query-time 10000

# 复杂查询环境
./target/debug/sealdb \
  --max-query-time 300000
```

## 监控和调试

### 调试模式

```bash
# 开启调试日志
./target/debug/sealdb \
  --log-level debug \
  --console-log

# 显示详细配置
./target/debug/sealdb \
  --config config/sealdb.toml \
  --show-config
```

### 生产监控

```bash
# 生产环境监控
./target/debug/sealdb \
  --config config/sealdb.toml \
  --log-level info \
  --log-file /var/log/sealdb.log \
  --daemon \
  --pid-file /var/run/sealdb.pid
```

## 最佳实践

1. **配置文件管理**：将配置文件放在版本控制中，但不要包含敏感信息
2. **参数优先级**：命令行参数 > 配置文件 > 默认值
3. **日志管理**：生产环境使用文件日志，开发环境使用控制台日志
4. **守护进程**：生产环境使用 `--daemon` 参数
5. **监控配置**：定期检查配置文件和日志文件
6. **备份策略**：定期备份配置文件和数据库