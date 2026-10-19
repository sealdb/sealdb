# CI/CD 设置指南

## 概述

SealDB 项目已经配置了完整的 CI/CD 流水线，包括代码质量检查、测试、代码覆盖率、安全审计和自动化发布。

## GitHub Actions 工作流

### 1. 主要 CI/CD 流水线 (`ci.yml`)

**触发条件：**
- 推送到 `main` 和 `develop` 分支
- 创建 Pull Request 到 `main` 和 `develop` 分支

**包含的任务：**

#### 测试任务
- **多版本 Rust 测试**：在 stable、1.70、1.75 版本上运行测试
- **代码格式化检查**：使用 `cargo fmt` 检查代码格式
- **代码质量检查**：使用 `cargo clippy` 检查代码质量
- **单元测试**：运行所有单元测试
- **集成测试**：运行所有集成测试

#### 代码覆盖率
- **覆盖率生成**：使用 `cargo-llvm-cov` 生成覆盖率报告
- **覆盖率上传**：上传到 Codecov
- **覆盖率制品**：保存覆盖率报告作为制品

#### 安全审计
- **依赖安全检查**：使用 `cargo audit` 检查安全漏洞

#### 构建任务
- **发布构建**：构建发布版本
- **制品上传**：上传构建产物

#### Docker 构建
- **Docker 镜像构建**：构建并推送 Docker 镜像
- **多标签支持**：支持 latest 和 commit SHA 标签

### 2. 代码质量检查 (`code-quality.yml`)

**包含的检查：**
- 代码格式化
- Clippy 检查
- 未使用依赖检查
- 安全漏洞检查
- 过时依赖检查
- 文档生成和链接检查

### 3. 发布工作流 (`release.yml`)

**触发条件：**
- 推送版本标签（如 `v1.0.0`）

**功能：**
- 多平台构建（Linux、Windows、macOS）
- 自动创建 GitHub Release
- 生成发布说明
- 上传构建产物

## 本地开发工具

### Makefile 命令

```bash
# 构建项目
make build

# 运行测试
make test

# 代码检查
make lint

# 格式化代码
make format

# 生成覆盖率报告
make coverage

# 生成文档
make docs

# 构建 Docker 镜像
make docker-build

# 运行 Docker 容器
make docker-run

# 停止 Docker 容器
make docker-stop

# 开发模式运行
make dev

# 安装开发依赖
make install-deps

# 安全检查
make security-check

# 依赖更新检查
make deps-check

# 完整检查
make check-all

# 发布准备
make release-prep
```

## Docker 支持

### Dockerfile 特性
- **多阶段构建**：优化镜像大小
- **非 root 用户**：提高安全性
- **健康检查**：自动健康监控
- **环境变量配置**：灵活的配置管理

### Docker Compose 配置
- **TiKV 集群**：完整的分布式存储环境
- **SealDB 应用**：主应用服务
- **监控栈**：Prometheus + Grafana
- **网络隔离**：安全的网络配置

## 测试覆盖

### 单元测试
- **配置模块**：测试配置加载和验证
- **连接管理**：测试连接池功能
- **优先级队列**：测试请求调度
- **SQL 解析器**：测试 SQL 解析功能
- **查询优化器**：测试优化规则
- **查询执行器**：测试执行计划

### 测试统计
- **总测试数**：36 个测试
- **通过率**：100%
- **覆盖模块**：所有核心模块

## 监控和可观测性

### Prometheus 配置
- **应用指标**：SealDB 性能指标
- **存储指标**：TiKV 集群指标
- **系统指标**：系统资源使用情况

### Grafana 仪表板
- **数据源配置**：Prometheus 数据源
- **仪表板配置**：自动加载仪表板

## 安全特性

### 代码安全
- **依赖审计**：定期检查安全漏洞
- **代码扫描**：静态代码分析
- **权限最小化**：Docker 容器使用非 root 用户

### 网络安全
- **端口暴露**：最小化端口暴露
- **网络隔离**：Docker 网络隔离
- **TLS 支持**：支持加密通信

## 部署指南

### 本地开发
```bash
# 克隆项目
git clone <repository-url>
cd sealdb

# 安装依赖
make install-deps

# 运行测试
make test

# 开发模式运行
make dev
```

### Docker 部署
```bash
# 构建镜像
make docker-build

# 启动完整环境
make docker-run

# 查看日志
docker-compose logs -f sealdb

# 停止环境
make docker-stop
```

### 生产部署
```bash
# 构建生产镜像
docker build -t sealdb:latest .

# 运行生产容器
docker run -d \
  --name sealdb \
  -p 4000:4000 \
  -v /path/to/config.toml:/app/config.toml:ro \
  -v /path/to/logs:/app/logs \
  sealdb:latest
```

## 故障排除

### 常见问题

1. **构建失败**
   - 检查 Rust 版本：`rustc --version`
   - 更新依赖：`cargo update`
   - 清理缓存：`cargo clean`

2. **测试失败**
   - 运行单个测试：`cargo test test_name`
   - 检查测试输出：`cargo test -- --nocapture`

3. **Docker 问题**
   - 检查 Docker 服务：`docker info`
   - 清理 Docker 缓存：`docker system prune`
   - 重新构建镜像：`docker build --no-cache`

4. **网络问题**
   - 检查端口占用：`netstat -tulpn | grep 4000`
   - 检查防火墙设置
   - 验证网络连接

### 日志查看
```bash
# 应用日志
docker-compose logs sealdb

# 存储日志
docker-compose logs tikv1

# 监控日志
docker-compose logs prometheus
```

## 贡献指南

### 提交前检查
```bash
# 运行完整检查
make check-all

# 确保测试通过
make test

# 检查代码质量
make lint
```

### 提交规范
- 使用清晰的提交信息
- 包含相关的测试
- 更新文档（如需要）
- 遵循代码风格指南

## 维护指南

### 定期维护任务
1. **依赖更新**：`make deps-check`
2. **安全审计**：`make security-check`
3. **代码质量检查**：`make lint`
4. **测试覆盖率检查**：`make coverage`

### 版本发布流程
1. 更新版本号
2. 运行完整测试：`make release-prep`
3. 创建版本标签：`git tag v1.0.0`
4. 推送标签：`git push origin v1.0.0`
5. 监控 CI/CD 流水线
6. 验证发布结果

## 联系和支持

如有问题或建议，请：
1. 查看项目文档
2. 提交 Issue
3. 参与讨论
4. 贡献代码