# SealDB 配置文件说明

SealDB 支持三种格式的配置文件：TOML、JSON 和 YAML。你可以根据个人喜好选择使用其中任何一种格式。

## 命令行参数

SealDB 提供了丰富的命令行参数，支持通过命令行覆盖配置文件中的设置。

### 基本用法

```bash
# 使用默认配置文件 (config.toml)
./target/debug/sealdb

# 指定配置文件
./target/debug/sealdb -c/--config config.json
./target/debug/sealdb -c/--config config.yaml
./target/debug/sealdb -c/--config config.toml

# 显示帮助信息
./target/debug/sealdb -h/--help

# 显示版本信息
./target/debug/sealdb -V/--version
./target/debug/sealdb --verbose-version
```

### 服务器配置参数

```bash
# 指定服务器地址和端口
./target/debug/sealdb -a/--host 127.0.0.1 -p/--port 8080

# 设置最大连接数
./target/debug/sealdb -n/--max-connections 2000

# 组合使用
./target/debug/sealdb -a/--host 0.0.0.0 -p/--port 4000 -n/--max-connections 1000
```

### 日志配置参数

```bash
# 设置日志级别
./target/debug/sealdb -l/--log-level debug
./target/debug/sealdb -l/--log-level info
./target/debug/sealdb -l/--log-level warn
./target/debug/sealdb -l/--log-level error

# 启用控制台日志输出
./target/debug/sealdb -o/--console-log

# 指定日志文件路径
./target/debug/sealdb -f/--log-file /var/log/sealdb.log
```

### 存储配置参数

```bash
# 指定 TiKV PD 端点
./target/debug/sealdb -e/--tikv-pd-endpoints 192.168.1.100:2379,192.168.1.101:2379

# 设置 TiKV 连接超时时间（毫秒）
./target/debug/sealdb -t/--tikv-connect-timeout 10000

# 设置 TiKV 请求超时时间（毫秒）
./target/debug/sealdb -r/--tikv-request-timeout 15000
```

### SQL 配置参数

```bash
# 设置最大查询执行时间（毫秒）
./target/debug/sealdb -q/--max-query-time 60000

# 设置最大内存使用量（字节）
./target/debug/sealdb -m/--max-memory-usage 2147483648  # 2GB
```

### 运行模式参数

```bash
# 以守护进程模式运行
./target/debug/sealdb -d/--daemon

# 指定 PID 文件路径
./target/debug/sealdb -d/--daemon -i/--pid-file /var/run/sealdb.pid
```

### 配置管理参数

```bash
# 显示当前配置
./target/debug/sealdb -c/--config config.toml -s/--show-config

# 生成默认配置文件
./target/debug/sealdb -g/--generate-config
```

### 参数优先级

命令行参数的优先级高于配置文件，具体优先级如下：

1. **命令行参数** - 最高优先级
2. **配置文件** - 中等优先级
3. **默认值** - 最低优先级

例如：
```bash
# 配置文件中的端口是 4000，但命令行指定了 8080
./target/debug/sealdb -c/--config config.toml -p/--port 8080
# 最终使用的端口是 8080
```

## 配置文件格式

### TOML 格式 (config.toml)

```toml
[server]
host = "0.0.0.0"
port = 4000
max_connections = 1000

[storage]
tikv_pd_endpoints = ["127.0.0.1:2379"]
tikv_connect_timeout = 6000
tikv_request_timeout = 12000

[sql]
max_query_time = 30000
max_memory_usage = 1073741824  # 1GB

[logging]
level = "info"
console = true
file = "logs/sealdb.log"
```

### JSON 格式 (config.json)

```json
{
  "server": {
    "host": "0.0.0.0",
    "port": 4000,
    "max_connections": 1000
  },
  "storage": {
    "tikv_pd_endpoints": ["127.0.0.1:2379"],
    "tikv_connect_timeout": 6000,
    "tikv_request_timeout": 12000
  },
  "sql": {
    "max_query_time": 30000,
    "max_memory_usage": 1073741824
  },
  "logging": {
    "level": "info",
    "console": true,
    "file": "logs/sealdb.log"
  }
}
```

### YAML 格式 (config.yaml)

```yaml
server:
  host: "0.0.0.0"
  port: 4000
  max_connections: 1000

storage:
  tikv_pd_endpoints:
    - "127.0.0.1:2379"
  tikv_connect_timeout: 6000
  tikv_request_timeout: 12000

sql:
  max_query_time: 30000
  max_memory_usage: 1073741824  # 1GB

logging:
  level: "info"
  console: true
  file: "logs/sealdb.log"
```

## 配置项说明

### server 配置
- `host`: 服务器监听地址，默认为 "0.0.0.0"（监听所有网络接口）
- `port`: 服务器监听端口，默认为 4000
- `max_connections`: 最大并发连接数，默认为 1000

### storage 配置
- `tikv_pd_endpoints`: TiKV PD 集群端点列表，支持多个 PD 节点以实现高可用
- `tikv_connect_timeout`: TiKV 连接超时时间（毫秒），默认为 6000
- `tikv_request_timeout`: TiKV 请求超时时间（毫秒），默认为 12000

### sql 配置
- `max_query_time`: 最大查询执行时间（毫秒），超过此时间的查询将被终止，默认为 30000
- `max_memory_usage`: 最大内存使用量（字节），默认为 1GB (1073741824)

### logging 配置
- `level`: 日志级别，可选值：debug, info, warn, error，默认为 "info"
- `console`: 是否将日志输出到控制台，默认为 true
- `file`: 日志文件路径，支持相对路径和绝对路径，默认为 "logs/sealdb.log"

## 自动检测文件格式

程序会根据文件扩展名自动检测配置文件格式：
- `.toml` - TOML 格式
- `.json` - JSON 格式
- `.yaml` 或 `.yml` - YAML 格式

如果没有扩展名，默认使用 TOML 格式解析。

## 生成默认配置

使用 `--generate-config` 参数可以生成三种格式的默认配置文件：

```bash
./target/debug/sealdb --generate-config
```

这将生成：
- `config.toml`
- `config.json`
- `config.yaml`

## 配置验证

程序会自动验证配置的有效性：

- **端口验证**：端口不能为 0
- **连接数验证**：最大连接数不能为 0
- **端点验证**：至少需要指定一个 TiKV PD 端点
- **日志级别验证**：必须是有效的日志级别（debug, info, warn, error）

如果配置无效，程序会显示相应的错误信息并退出。

## 生产环境建议

1. **安全性**：不要将包含敏感信息的配置文件提交到版本控制系统
2. **备份**：定期备份配置文件
3. **监控**：监控配置文件的变化
4. **文档**：记录配置项的修改原因和影响
5. **守护进程**：在生产环境中使用 `--daemon` 参数以守护进程模式运行
6. **日志管理**：配置适当的日志级别和日志文件路径