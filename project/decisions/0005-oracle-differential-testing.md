# ADR-0005: 以 TiDB 为 Oracle 的差分测试

- 状态：**accepted**
- 日期：2026-10-19
- 关联：requirements §5；testing.md；risks R5/R15

## 背景

TiDB 计算层行为细节极多（类型、排序规则、NULL、错误码、计划形状、事务冲突）。  
仅靠单元测试/人工 review 无法证明“与 TiDB 一致”；需要可重复、可量化的对照。

## 决策

建立 **Go Oracle Harness**，以 TiDB v8.5.8 参考实现生成期望输出；Rust 侧做差分比较。

### 核心原则

1. **Oracle 只读**：不修改 TiDB 参考仓库。
2. **输出稳定 JSON**：归一化掉 ID/时间/浮点/顺序等非确定性。
3. **金样人工 review**：不把 TiDB 的 bug 当标准；有疑问记录版本/issue。
4. **语料冻结**：每个阶段的分母写入 `corpus.lock.json`，避免通过率被稀释。
5. **分层差分**：AST / Plan / Result / Error / Txn 分别比较，定位到层。

### 工具

```text
tools/oracle/                 # Go module（replace 本地 TiDB）
  cmd/tidb-oracle/main.go
  internal/parse|plan|execute|txn

tests/conformance/            # Rust 金样测试
testdata/sql|schema|golden    # 输入/期望
project/reports/phase-XX/     # Gate 证据
```

### 比较规则

- AST：去 text/offset，字段排序，类型名一致。
- Plan：去 plan ID/cost 浮点/runtime stats；比较算子类型、子节点、关键属性。
- Result：按类型序列化；无 ORDER BY 按多重集；有 ORDER BY 严格顺序。
- Error：code + SQLSTATE + 归一化 message。

## 后果

### 正面

- 可量化进度（Parser/Plan/Result 通过率）。
- 回归可重复；避免“看起来对”。
- 能发现跨层问题（如类型错误导致计划/结果偏差）。

### 负面 / 代价

- 需要维护 Go 工具链与 Oracle 代码。
- 归一化规则本身可能掩盖差异；需 review 与白名单控制。
- 真实 TiKV 集成测试较慢；需要分层运行。

## 备选方案

| 方案 | 结论 |
|---|---|
| 只用单元测试 | 覆盖不足，拒绝 |
| 人工对比 mysql 输出 | 不可重复，拒绝 |
| 直接链接 TiDB Go 代码（cgo/FFI） | 构建复杂，拒绝 |
| 只做结果差分，不做计划 | 无法定位优化器问题，不足 |

## 验证方式

- Phase 0：Oracle 能 parse `select 1` 并输出 JSON。
- Phase 2：Parser 通过率 ≥ 60%。
- Phase 4/5：Plan/Result 差分报告归档。
- Phase Gate：`phase_gate.sh` 自动跑对应 suite。

## 参考

- `testing.md`
- `project/scripts/oracle_diff.sh`
- TiDB `pkg/testkit`、`pkg/store/mockstore`
