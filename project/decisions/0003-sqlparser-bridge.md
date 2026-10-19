# ADR-0003: sqlparser-rs + TiDB AST Lowering

- 状态：**accepted**
- 日期：2026-10-19
- 关联：requirements C3；architecture §4

## 背景

TiDB 使用 goyacc 文法（`pkg/parser/parser.y`）生成 LALR(1) 解析器，产出 `pkg/parser/ast` 的 `StmtNode/ExprNode`。  
用户明确要求：**继续使用 sqlparser-rs**，不移植 goyacc。

问题：sqlparser-rs 的 AST 形状与 TiDB AST 不同；Planner/Executor 的上游代码依赖 TiDB AST 的字段、Visitor、Flag、Restore。如果让 Planner 直接消费 sqlparser AST，会导致后续大量改写，违背“核心逻辑一致”。

## 决策

采用**两段式**：

```text
SQL Text
  → [可选文本预处理] → sqlparser-rs (TiDBDialect)
  → sqlparser AST
  → [AST Lowering] → TiDB 形状 AST (`pkg/parser/ast`)
  → Preprocess / Planner / Executor
```

具体：

1. `pkg/parser/sqlparser_bridge/dialect.rs`：实现 `sqlparser::dialect::Dialect`，覆盖 MySQL/TiDB 词法差异。
2. `pkg/parser/sqlparser_bridge/lower.rs`：把 `sqlparser::ast::{Statement, Expr, ...}` 转成 `pkg::parser::ast::*`。
3. `pkg/parser/parser.rs`：对外 `parse_sql`/`parse_one_stmt`，负责多语句、depth 检查、错误映射、`SetFlag`。
4. `pkg/parser/format/`：实现 `Restore`，保证 AST → SQL 可 round-trip。
5. 文本预处理只处理 sqlparser 无法表达的 TiDB 特有语法；**禁止**用正则实现 SQL 语义。

## 后果

### 正面

- 满足“使用主流 sqlparser-rs”的约束。
- Planner/Executor 可以按 TiDB 源码移植，减少认知负担。
- AST 层可独立做 Oracle 差分，定位问题清晰。

### 负面 / 代价

- 多一次 AST 转换，解析性能略低（可通过流式转换/缓存缓解）。
- 需要维护两套 AST 的映射，新增语法要改 bridge。
- sqlparser-rs 的覆盖范围决定 Parser 上限；缺口需要预处理/子解析器/上游 PR。

## 备选方案

| 方案 | 结论 |
|---|---|
| 直接让 Planner 用 sqlparser AST | 后续改写量巨大，拒绝 |
| Fork sqlparser-rs 产出 TiDB AST | 长期维护成本高；短期仅允许 patch |
| 移植 goyacc 文法到 Rust（lalrpop 等） | 违背用户约束，拒绝 |
| 只做 SQL 文本翻译到 MySQL 再解析 | 语义/位置/错误丢失，拒绝 |

## 验证方式

- Phase 2 Parser Oracle 通过率 ≥ 60%，Phase 5 ≥ 80%。
- 所有已支持语句 `Restore` round-trip。
- AST depth/错误位置/错误码与 TiDB 接近。
- 对缺口维护 `sqlparser-gaps.md`（可按需新增）。

## 参考

- `learn/02-parser.md`
- `pkg/parser/ast/ast.go`
- `architecture.md` §4
