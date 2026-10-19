---
name: port-tidb-file
description: 将 TiDB Go 源文件按目录/文件对齐规则移植为 Rust，并完成测试、Oracle 用例与进度更新。适用于“移植 pkg/.../foo.go”这类任务。
---

# Skill: 移植一个 TiDB 文件

## 适用

用户要求“移植 `pkg/.../foo.go` 到 Rust”“补齐某个 TiDB 包”时。

## 前置阅读

1. `project/requirements.md`（硬约束）
2. `project/conventions.md`（状态登记、错误、测试）
3. `project/architecture.md`（目录对齐、依赖方向）
4. 对应阶段的 `project/roadmap.md` 段落

## 步骤

### 1. 定位上游

```bash
cd /home/wslu/work/github/db/pingcap/tidb
sed -n '1,240p' pkg/<path>/<file>.go
rg "FuncName\(" pkg --glob '*.go'   # 调用方
rg "FuncName" pkg --glob '*_test.go' # 测试
```

记录：

- 输入/输出、错误分支、并发假设；
- 依赖的类型/接口（先在 Rust 侧确认存在）；
- 是否为生成代码（`*_generated.go`、`parser.go`）。

### 2. 确认 Rust 落点

- 路径：`pkg/<path>/<file>.rs`（同名）。
- 若目录不存在：先建 `mod.rs`，在父 `mod.rs`/`lib.rs` 注册。
- 若文件是桥接/语言差异：放 `pkg/parser/sqlparser_bridge/` 或按 ADR 处理。

### 3. 登记状态

在 `project/parity/status.json` 增加条目（源码中不写来源标记）：

```json
"pkg/<path>/<file>.go": {
  "status": "stub",
  "rust": "pkg/<path>/<file>.rs",
  "phase": 2,
  "notes": "public API skeleton"
}
```

### 4. 移植

- 先类型/结构体/枚举，再函数。
- 保持函数职责与上游可对应；Rust 命名用 snake_case。
- 错误路径必须显式；禁止 `unwrap()` 进入生产路径。
- 未实现分支：`return Err(Error::Unsupported(...))` 或 `todo!()`（仅 stub 文件），并在 manifest 登记。
- 不要顺手优化算法；优化另开任务/ADR。

### 5. 测试

- 翻译上游表驱动测试到 `#[cfg(test)] mod tests`。
- 覆盖正常/边界/错误。
- 涉及 AST/Plan/Result 的，加入 Oracle 用例（见 `oracle-diff` Skill）。

### 6. 验证

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test <module>::tests
python3 project/scripts/parity_check.py --tidb-root <tidb> --rust-root .
```

### 7. 更新

- `project/progress.md`：文件/包状态、测试证据。
- `project/parity-matrix.md`：如包级状态变化。
- 如遇偏差：新增/更新 ADR 或 `PARITY_NOTES`。

## 禁止

- ❌ 不读上游就按函数名猜实现。
- ❌ 改 TiDB 参考仓库。
- ❌ 用正则/字符串替换实现 SQL 语义。
- ❌ 静默 `todo!()`/空实现。
- ❌ 删除失败测试来“通过”。
- ❌ 顺手重构无关文件。

## 完成标准

- [ ] 文件存在且路径同名。
- [ ] `project/parity/status.json` 已登记。
- [ ] 核心逻辑与上游一致；差异有说明。
- [ ] 测试通过；Oracle 用例（如适用）通过。
- [ ] 进度/矩阵已更新。
- [ ] 源码中没有来源/移植标记。
