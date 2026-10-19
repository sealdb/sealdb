# 测试与 Oracle 差分（Testing）

> 核心思想：**TiDB 是行为 Oracle**。Rust 实现是否正确，不靠“看起来一样”，靠同一输入下的可比较输出。  
> 相关：`project/scripts/oracle_diff.sh`、`tools/oracle/`、`testdata/`。

---

## 1. 测试金字塔

```text
                 ┌─────────────────────┐
                 │  Differential/E2E   │  同一 SQL：TiDB vs Rust
                 ├─────────────────────┤
                 │  Integration        │  真实 PD/TiKV / docker-compose
                 ├─────────────────────┤
                 │  Golden/Conformance │  AST / Plan / Result 金样
                 ├─────────────────────┤
                 │  Property/Fuzz      │  编码 round-trip、Parser 差分
                 ├─────────────────────┤
                 │  Unit               │  函数/模块级
                 └─────────────────────┘
```

| 层 | 工具 | 运行频率 | 作用 |
|---|---|---|---|
| Unit | `cargo test` | 每次提交 | 快速反馈 |
| Property | `proptest` | 每次提交/夜间 | 编码、类型转换 |
| Golden | Rust 测试 + JSON 金样 | 每次提交 | 防止行为回退 |
| Oracle 差分 | Go harness + Rust harness | 每日/阶段 Gate | 与 TiDB 对齐 |
| Integration | docker-compose（PD+TiKV） | 夜间/阶段 Gate | 真实分布式行为 |
| Fuzz | `cargo-fuzz` / 文本差分 | 夜间 | Parser 健壮性 |
| Benchmark | `criterion` | 夜间/版本节点 | 性能回归 |

---

## 2. Oracle Harness（Go）

### 2.1 位置与原则

```text
tools/oracle/
├── go.mod                 # module sealdb/oracle
├── cmd/tidb-oracle/main.go
├── internal/parse/        # ParseSQL → AST JSON
├── internal/plan/         # Preprocess + Optimize → Plan JSON / EXPLAIN
├── internal/execute/      # 执行 SQL → 结果 JSON
└── testdata/              # 输入语料
```

- 它是**独立 Go module**，通过 `replace github.com/pingcap/tidb => ../../pingcap/tidb` 引用本地源码。
- **禁止修改** TiDB 参考仓库；Oracle 只读。
- Oracle 输出稳定 JSON；Rust 侧只消费 JSON，不链接 Go。

### 2.2 模式

| 模式 | 输入 | 输出 | 用途 |
|---|---|---|---|
| `parse` | SQL 文本/文件 | `[]StmtNode` 归一化 JSON + error | Parser 对齐 |
| `restore` | SQL | `Restore` 后的 SQL 文本 | round-trip |
| `plan` | SQL + schema + stats | 逻辑/物理计划 JSON + EXPLAIN | Planner 对齐 |
| `execute` | SQL + schema + data | 列定义 + 行（typed） | Executor 对齐 |
| `txn` | 脚本（BEGIN/UPDATE/...） | 每步结果/错误 | 事务对齐 |
| `schema` | DDL 脚本 | InfoSchema 快照 JSON | 元数据对齐 |

### 2.3 归一化（必须实现）

| 对象 | 去掉/归一化 |
|---|---|
| AST | `text`/`originalText`/offset；空字段统一省略；map key 排序 |
| Plan | plan ID、column ID、cost 浮点（保留 6 位/可关闭）、时间戳、`actRows`、runtime info |
| EXPLAIN | 只比较树形与关键字段；可选 `--explain-verbose` |
| Result | 按类型序列化（整型/Decimal/字符串/时间/JSON）；不比较显示格式 |
| 行序 | 无 `ORDER BY` 时按多重集比较；有 `ORDER BY` 时严格顺序 |
| 错误 | `code` + `sqlstate` + 归一化 message（去路径/ID） |
| 浮点 | 默认 `abs/rel <= 1e-6`；或统一格式化后比较 |
| 时间 | 固定时区（UTC）、固定 `time_zone` 会话变量 |

### 2.4 示例命令

```bash
cd tools/oracle

# 解析
go run ./cmd/tidb-oracle parse \
  --sql "select id, name from t where id = 1" \
  --out /tmp/ast.json

# 计划（固定统计信息）
go run ./cmd/tidb-oracle plan \
  --file testdata/plan_cases.sql \
  --schema testdata/schema.sql \
  --stats fixed \
  --out /tmp/plan.json

# 执行（mockstore/unistore，无需真实 TiKV）
go run ./cmd/tidb-oracle execute \
  --file testdata/exec_cases.sql \
  --schema testdata/schema.sql \
  --data testdata/data.sql \
  --store mock \
  --out /tmp/exec.json

# 执行（真实 TiKV）
go run ./cmd/tidb-oracle execute \
  --file testdata/exec_cases.sql \
  --store "tikv://127.0.0.1:2379" \
  --out /tmp/exec-tikv.json
```

> `--store mock` 使用 TiDB `pkg/store/mockstore` / `pkg/testkit`，保证确定性；真实 TiKV 用于集成验证。

---

## 3. Rust 侧差分 Harness

### 3.1 位置

```text
tests/
├── conformance/           # 金样测试
│   ├── parser/
│   ├── plan/
│   ├── executor/
│   └── txn/
├── integration/           # 真实 PD/TiKV
└── common/                # 归一化/比较工具
testdata/
├── sql/                   # 输入 SQL
├── schema/                # 建表/数据
└── golden/                # Oracle 生成的期望 JSON
```

### 3.2 比较器

Rust 侧读取 Oracle JSON，与本地输出比较：

```rust
pub fn assert_ast_equal(expected: &serde_json::Value, actual: &serde_json::Value);
pub fn assert_plan_equal(expected: &PlanSnapshot, actual: &PlanSnapshot, opts: &DiffOpts);
pub fn assert_result_equal(expected: &ResultSet, actual: &ResultSet, opts: &DiffOpts);
pub fn assert_error_equal(expected: &SqlError, actual: &SqlError);
```

规则：

- 缺字段/多字段都算差异（除白名单）。
- 浮点/顺序/时间按 §2.3 规则。
- 差异输出必须可读：`path: expected vs actual`，并写 `project/reports/.../diff.json`。

### 3.3 金样生成流程

1. 在 `testdata/sql/` 添加 SQL 用例。
2. 运行 Oracle 生成 JSON 到 `testdata/golden/<suite>/`。
3. 人工 review 金样（防止把 TiDB 的异常行为当标准）。
4. Rust 测试读取金样并比较。
5. 金样变更必须单独提交，说明原因。

---

## 4. 语料（Corpus）

### 4.1 来源

| 来源 | 用途 | 备注 |
|---|---|---|
| `pkg/parser/parser_test.go` + `ast/*_test.go` | Parser | 表驱动用例；优先提取 SELECT/DML/DDL 基础 |
| `pkg/planner/core/testdata/*.json` | Planner | `plan_suite_*`、`index_merge_*` 等 |
| `pkg/planner/core/casetest` | Planner | 集成计划用例 |
| `pkg/executor/testdata/*.json` | Executor | prepare/slow_query 等；执行结果需补充 |
| `pkg/executor/**/*_test.go` | Executor | 表驱动结果用例 |
| `docs/design/*.md` | 设计意图 | 不作为金样，作为理解材料 |
| 自建 `testdata/sql/` | 竖切/Gate | 每个阶段补充 |

### 4.2 语料分层

| Suite | 内容 | 首次启用 |
|---|---|---|
| `parser-smoke` | 20–50 条基础 SQL | Phase 0/2 |
| `parser-core` | SELECT/DML/DDL 基础、表达式、Join、子查询 | Phase 2 |
| `plan-single-table` | 单表访问路径/谓词/排序/聚合 | Phase 4 |
| `plan-join` | 两表/三表 Join、JoinReorder | Phase 4 |
| `exec-single-table` | 点查/范围/过滤/排序/聚合 | Phase 5 |
| `exec-join-agg` | Join/Agg/SetOp/Window | Phase 5/8 |
| `txn-basic` | BEGIN/COMMIT/ROLLBACK/冲突 | Phase 6 |
| `ddl-basic` | CREATE/ALTER/DROP/索引 | Phase 8 |
| `protocol` | 握手/查询/结果集 | Phase 9 |

> 每个 Stage 的分母在 `project/reports/phase-XX/corpus.lock.json` 冻结，避免“通过率”被动态稀释。

---

## 5. 差分执行策略

### 5.1 只读差分（推荐优先）

- 同一 TiKV 集群，同一 schema/data 快照。
- TiDB 与 Rust 都只读，避免 DDL/写入互相干扰。
- 比较：结果集、计划、错误。

### 5.2 写入差分（谨慎）

- 使用**独立 keyspace/集群**或每轮重建数据。
- 顺序：TiDB 执行 → 记录结果 → 清空/重建 → Rust 执行 → 比较。
- 事务并发测试需单独设计（锁/冲突非确定性）。

### 5.3 计划差分

- 固定统计信息（导入同一份 stats 或使用固定 mock）。
- 关闭非确定性规则（若上游允许）并在报告中注明。
- 比较：算子类型、子节点顺序、关键属性（access path、join type、task type）。
- 不比较：plan ID、cost 浮点（可配置容差）、runtime stats。

---

## 6. 集成测试（PD/TiKV）

### 6.1 环境

```yaml
# docker/test/docker-compose.yml（Phase 0 固定版本）
services:
  pd:
    image: pingcap/pd:v8.5.0
  tikv:
    image: pingcap/tikv:v8.5.0
  # 可选：tidb 作为 Oracle
  tidb:
    image: pingcap/tidb:v8.5.0
```

> 禁止使用 `:latest`；版本写入 `progress.md` 与测试报告。

### 6.2 用例

| 用例 | 验证 |
|---|---|
| PD TSO | 连续取 TS 单调递增 |
| Region 路由 | 写入后 LocateKey 命中正确 Region |
| 乐观事务 | 两事务写冲突 → 一成一败 |
| 悲观事务 | `SELECT FOR UPDATE` 阻塞/超时 |
| 2PC | 提交后 TiDB 可读；回滚后不可见 |
| Schema | Rust 读 TiDB 建表；TiDB 读 Rust 建表（Phase 8） |
| Cop | Selection/Projection/Limit 下推结果一致 |

### 6.3 交叉验证矩阵

| 写入方 | 读取方 | 阶段 |
|---|---|---|
| TiDB | Rust | Phase 3+ |
| Rust | TiDB | Phase 5+ |
| Rust | Rust | Phase 5+ |
| TiDB | TiDB（基线） | 始终 |

---

## 7. 属性测试与 Fuzz

| 对象 | 属性 |
|---|---|
| `codec.Encode*` | `decode(encode(x)) == x`；字节序与数值序一致 |
| `tablecodec` | 行/索引 key 可逆；前缀范围正确 |
| `Datum` 比较 | 反对称、传递、NULL 语义 |
| Parser | 对随机合法 SQL，Rust 与 TiDB 的 AST 归一化一致（差分 fuzz） |
| 计划 | 同一 SQL 重复优化结果稳定（去掉 ID/时间） |

Fuzz 目标：

```bash
cargo +nightly fuzz run parser_diff
cargo +nightly fuzz run codec_roundtrip
```

---

## 8. 性能测试（后置但预留）

| 基准 | 指标 | 阶段 |
|---|---|---|
| PointGet | p50/p99 latency | Phase 5 |
| TableScan | rows/s | Phase 5 |
| HashJoin | build/probe 时间 | Phase 5 |
| HashAgg | 分组吞吐 | Phase 5 |
| TPC-H Q1/Q6 | 端到端 | Phase 7+ |
| TPC-C | tpmC | Phase 7+ |

工具：`criterion`；对比 TiDB 时固定硬件/数据/并发，报告写入 `project/reports/perf/`。

---

## 9. CI 与 Gate

### 9.1 每次提交（Fast）

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --lib
python3 project/scripts/parity_check.py --strict-headers
```

### 9.2 每日/夜间（Full）

```bash
cargo test --workspace
./project/scripts/oracle_diff.sh --suite parser-core
./project/scripts/oracle_diff.sh --suite plan-single-table
docker compose -f docker/test/docker-compose.yml up -d
cargo test --workspace --features integration
```

### 9.3 阶段 Gate

```bash
./project/scripts/phase_gate.sh 02
```

`phase_gate.sh` 读取阶段清单，运行对应 suite，生成 `project/reports/phase-XX/summary.md`，失败即非零退出。

---

## 10. 新增测试的步骤

1. 在 `testdata/sql/<suite>/` 添加 `.sql`（一行一例或用 `;` 分隔）。
2. 运行 Oracle 生成金样（`./project/scripts/oracle_diff.sh --update-golden --suite <suite>`）。
3. 人工 review 金样 diff（确认是 TiDB 正确行为）。
4. 在对应 Rust 模块加 `#[test]`，引用金样。
5. 更新 `progress.md` 的测试计数/通过率。
6. 若发现 TiDB 行为与文档不符，记录到 `project/notes/`，不要擅自“修正”金样。

---

## 11. 测试反模式

- ❌ 只断言 `is_ok()`，不校验结果。
- ❌ 用字符串格式化结果比较（`SELECT 1.0` vs `1.00`）。
- ❌ 无 `ORDER BY` 却按行序比较。
- ❌ 把运行时时间/随机 ID 写入金样。
- ❌ 删除失败用例来“通过”。
- ❌ 用 mock 绕过真实 TiKV 后声称“集成通过”。
- ❌ 未冻结语料就宣称通过率。
