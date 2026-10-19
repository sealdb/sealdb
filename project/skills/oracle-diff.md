---
name: oracle-diff
description: 运行 Go Oracle 生成 TiDB 的 AST/Plan/Result 期望，并与 Rust 输出做归一化差分，定位第一处不一致。适用于“结果不一致/计划不一致/Parser 不过”。
---

# Skill: Oracle 差分

## 适用

- Rust 与 TiDB 的 AST/Plan/Result/Error 不一致；
- 新增语料/金样；
- 阶段 Gate 的一致性测试。

## 前置

- 已安装 Go（与 TiDB `go.mod` 兼容）；
- `tools/oracle/` 存在且可构建；
- 测试数据/schema 明确。

## 步骤

### 1. 明确比较层

| 现象 | 先跑 | 状态 |
|---|---|---|
| 解析失败/AST 不同 | `-sql` / `-suite` | Phase 0 可用 |
| 计划形状/选路不同 | plan 模式 | Phase 4 |
| 行/列/类型不同 | execute 模式 | Phase 5 |
| 错误码/消息不同 | error 模式 | Phase 2+ |
| 事务行为不同 | txn 模式 | Phase 6 |

### 2. 生成期望

```bash
cd tools/oracle
go run ./cmd/tidb-oracle -sql "SELECT ..." -pretty > /tmp/expected.ast.json
go run ./cmd/tidb-oracle -suite testdata/parser-smoke -pretty > /tmp/expected.suite.json
# plan/execute 模式将在 Phase 4/5 提供；当前先做 AST 差分。
```

### 3. 生成实际

```bash
cargo run --bin tidb-server -- --parse "SELECT ..." --ast-json > /tmp/actual.ast.json
cargo run --bin tidb-server -- --explain "SELECT ..." --json > /tmp/actual.plan.json
cargo run --bin tidb-server -- --execute "SELECT ..." --result-json > /tmp/actual.exec.json
```

### 4. 归一化后 diff

```bash
./project/scripts/normalize_json.py /tmp/expected.ast.json > /tmp/e.json
./project/scripts/normalize_json.py /tmp/actual.ast.json   > /tmp/a.json
diff -u /tmp/e.json /tmp/a.json
```

规则见 `project/testing.md` §2.3：

- AST：去 text/offset；
- Plan：去 ID/cost 浮点/runtime stats；
- Result：按类型、行序规则；
- Error：code + SQLSTATE + 归一化 message。

### 5. 定位第一处差异

- 从顶层类型开始，递归到字段；
- 记录：`path`、expected、actual、对应上游文件/函数；
- 判断：Parser lowering / Preprocess / RBO / CBO / Executor / 类型系统。

### 6. 修复与回归

- 修对应层；不要在测试/归一化里掩盖差异。
- 重跑该用例 + 相邻用例（防止回归）。
- 必要时把用例加入 `testdata/sql/` 并生成金样。

## 常见差异与归属

| 差异 | 层 | 常见原因 |
|---|---|---|
| AST 节点类型不同 | Parser/Lowering | sqlparser 映射错误 |
| AST 字段缺失 | Parser/Lowering | 漏字段/默认值 |
| 逻辑计划多/少算子 | PlanBuilder/RBO | 规则未跑/顺序错 |
| 物理算子不同 | CBO | stats/cost/属性 |
| 行数/值不同 | Executor/表达式 | NULL/类型/Cast/`sel` |
| 错误码不同 | 错误映射 | errno/SQLSTATE |

## 禁止

- ❌ 修改期望 JSON 来“通过”。
- ❌ 在归一化中删掉真实差异。
- ❌ 只比较 `is_ok()`。
- ❌ 用 mock 结果替代真实执行后声称通过。

## 完成标准

- [ ] 差异定位到具体层/文件。
- [ ] 修复后该用例 + 相邻用例通过。
- [ ] 金样（如新增）经人工 review。
- [ ] 报告写入 `project/reports/`。
