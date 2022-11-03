# SealDB 配置文件模板

本目录包含了 SealDB 的配置文件模板，支持三种格式：TOML、JSON 和 YAML。

## 文件说明

- `sealdb.toml` - TOML 格式配置文件模板
- `sealdb.json` - JSON 格式配置文件模板
- `sealdb.yaml` - YAML 格式配置文件模板

## 使用方法

1. **选择配置文件格式**：根据个人喜好选择 TOML、JSON 或 YAML 格式
2. **复制模板文件**：将选择的模板文件复制到项目根目录
3. **修改配置**：根据实际需求修改配置项
4. **启动服务**：使用 `--config` 参数指定配置文件

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

## 示例命令

```bash
# 使用 TOML 配置文件
./target/debug/sealdb --config config/sealdb.toml

# 使用 JSON 配置文件
./target/debug/sealdb --config config/sealdb.json

# 使用 YAML 配置文件
./target/debug/sealdb --config config/sealdb.yaml

# 显示配置内容
./target/debug/sealdb --config config/sealdb.toml --show-config
```

## 注意事项

1. **文件格式**：程序会根据文件扩展名自动检测配置文件格式
2. **注释**：TOML 和 YAML 格式支持注释，JSON 格式不支持注释
3. **数据类型**：确保配置项的数据类型正确（字符串、数字、布尔值等）
4. **路径**：日志文件路径支持相对路径和绝对路径
5. **网络**：确保 TiKV PD 端点地址正确且可访问

## 生产环境建议

1. **安全性**：不要将包含敏感信息的配置文件提交到版本控制系统
2. **备份**：定期备份配置文件
3. **监控**：监控配置文件的变化
4. **文档**：记录配置项的修改原因和影响