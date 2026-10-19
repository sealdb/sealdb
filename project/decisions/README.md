# 架构决策记录（ADR）

> 每个 ADR 记录一个**难以逆转**或**影响多模块**的决策。  
> 状态：`proposed` / `accepted` / `superseded` / `rejected`。  
> 新 ADR 编号递增；不要删除旧 ADR，用 `superseded by ADR-xxxx` 标记。

## 模板

```markdown
# ADR-XXXX: 标题

- 状态：proposed
- 日期：YYYY-MM-DD
- 决策者：
- 关联：requirements Cx / roadmap Phase x

## 背景
## 决策
## 后果
### 正面
### 负面 / 代价
## 备选方案
## 验证方式
## 参考
```

## 索引

| ADR | 标题 | 状态 |
|---|---|---|
| [0001](./0001-rust-rewrite-reuse-pd-tikv.md) | Rust 重写计算层并复用 PD/TiKV | accepted |
| [0002](./0002-single-crate-first.md) | 单 crate 优先，保持 TiDB 目录对齐 | accepted |
| [0003](./0003-sqlparser-bridge.md) | sqlparser-rs + TiDB AST lowering | accepted |
| [0004](./0004-tikv-client-strategy.md) | TiKV 客户端：tikv-client 优先，kvproto 兜底 | proposed |
| [0005](./0005-oracle-differential-testing.md) | 以 TiDB 为 Oracle 的差分测试 | accepted |
| [0006](./0006-toolchain-edition-msrv.md) | Edition 2021 / MSRV 1.80.1 | accepted |
| [0007](./0007-legacy-code-disposition.md) | 旧 Rust 代码处置 | proposed |
