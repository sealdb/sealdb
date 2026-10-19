# ADR-0001: Rust 重写 TiDB 计算层并复用 PD/TiKV

- 状态：**accepted**
- 日期：2026-10-19
- 关联：requirements C3/C4；architecture §1

## 背景

目标是获得一个 Rust 实现的 TiDB 兼容计算节点，同时不重写分布式存储/调度：

- PD 提供 TSO、Region 元数据、调度；
- TiKV 提供 KV、MVCC、Percolator 2PC、Coprocessor；
- TiDB 计算层负责协议、解析、优化、执行、元数据/事务编排。

如果连存储层一起重写，项目规模与风险会成倍增加，且偏离“复用成熟能力”的初衷。

## 决策

1. **只重写 TiDB 计算层**；PD/TiKV **源码不修改**，仅作为外部服务使用。
2. Rust 节点通过 `pkg/store` 访问 TiKV/PD；禁止在 Planner/Executor 直接发 RPC。
3. 行为基准固定为 **TiDB v8.5.8**。
4. 目录/文件名对齐 TiDB，算法与调用顺序尽量一致。

## 后果

### 正面

- 复用成熟的分布式存储、事务、调度能力。
- 可以把精力集中在 SQL 语义/优化器/执行器。
- 能与 TiDB 共用同一 TiKV 集群做交叉验证。

### 负面 / 代价

- 必须精确理解 TiDB 对存储的契约（Key 编码、2PC、Region 路由、Schema 版本）。
- Rust 客户端生态（client-rust）能力可能不如 Go client-go，需要适配/兜底。
- TiDB 与本地 PD/TiKV 版本需对齐（见 R4）。

## 备选方案

| 方案 | 结论 |
|---|---|
| 重写存储 + 计算 | 范围过大，拒绝 |
| Fork TiDB 改 Rust 存储适配 | 不现实，拒绝 |
| 只做 SQL 代理/翻译层 | 无法达到计算层等价，拒绝 |

## 验证方式

- Phase 0：Rust 能连 PD/TiKV，完成 TSO/Region/事务/Cop 冒烟。
- Phase 3+：Rust 读取 TiDB 创建的 schema/数据。
- Phase 5+：Rust 写、TiDB 读；TiDB 写、Rust 读。

## 参考

- `learn/01-overview-and-pipeline.md`
- `pkg/kv/kv.go`、`pkg/store/driver/tikv_driver.go`
- `requirements.md` C4
