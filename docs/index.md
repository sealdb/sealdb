# SealDB 文档索引

欢迎来到 SealDB 文档中心！这里包含了项目的所有技术文档和指南。

## 📚 快速开始

- **[快速开始指南](quick_start.md)** - 快速上手 SealDB
- **[开发指南](development_guide.md)** - 开发环境搭建和开发流程

## 🏗️ 架构设计

- **[系统架构](architecture.md)** - SealDB 整体架构设计
- **[SQL解析器架构](sql_parser.md)** - SQL解析器的设计和实现
- **[SQL优化架构](sql_optimizer.md)** - RBO和CBO优化架构
- **[线程池设计](thread_pool_design.md)** - 线程池的设计和实现

## 🔧 API 参考

- **[API参考文档](api_reference.md)** - 完整的API接口文档

## 📋 实现总结



## 📖 其他文档

- **[README](../README.md)** - 项目主页和概述

## 🔍 文档分类

### 按功能模块

| 模块 | 相关文档 |
|------|----------|
| SQL解析器 | [sql_parser.md](sql_parser.md) |
| SQL优化器 | [sql_optimizer.md](sql_optimizer.md) |
| 系统架构 | [architecture.md](architecture.md) |
| 线程池 | [thread_pool_design.md](thread_pool_design.md) |

### 按文档类型

| 类型 | 相关文档 |
|------|----------|
| 快速开始 | [quick_start.md](quick_start.md) |
| 开发指南 | [development_guide.md](development_guide.md) |
| 架构设计 | [architecture.md](architecture.md), [sql_parser.md](sql_parser.md), [sql_optimizer.md](sql_optimizer.md) |
| API文档 | [api_reference.md](api_reference.md) |

## 📝 文档维护

### 文档命名规范

- 所有文档使用小写字母命名
- 单词间用下划线分隔
- 文件名应具有描述性

### 文档结构

```
docs/
├── index.md                           # 文档索引（本文件）
├── README.md                          # 文档中心说明
├── quick_start.md                     # 快速开始
├── development_guide.md               # 开发指南
├── architecture.md                    # 系统架构
├── sql_parser.md                      # SQL解析器架构
├── sql_optimizer.md                   # SQL优化架构
├── thread_pool_design.md             # 线程池设计
└── api_reference.md                  # API参考
```

### 更新日志

- **2024-08-03**: 重新组织文档结构，统一命名规范
- **2024-08-03**: 完成SQL解析器和优化器架构文档
- **2024-08-03**: 添加解析器实现总结文档

## 🤝 贡献指南

如果您想为文档做出贡献，请：

1. 遵循现有的命名规范
2. 使用清晰的标题和结构
3. 添加适当的代码示例
4. 保持文档的及时更新

## 📞 获取帮助

如果您在使用过程中遇到问题：

1. 查看相关文档
2. 检查 [GitHub Issues](https://github.com/sealdb/sealdb/issues)
3. 提交新的 Issue 或 Pull Request

---

*最后更新: 2024-08-03*