---
name: parity-audit
description: 审计 Rust 仓库与 TiDB 参考仓库的目录/文件对齐状态，生成报告并定位缺失、无状态头、非法 stub 等问题。适用于“检查对齐/缺哪些文件/Phase Gate 前审计”。
---

# Skill: 目录/文件对齐审计

## 适用

- 阶段开始/结束时的 parity 检查；
- 怀疑漏移植文件；
- CI 报 `invalid`/`missing-dir`。

## 命令

```bash
python3 project/scripts/parity_check.py \
  --tidb-root /home/wslu/work/github/db/pingcap/tidb \
  --rust-root . \
  --json > project/reports/parity.json
```

阶段专用：

```bash
python3 project/scripts/parity_check.py \
  --tidb-root /home/wslu/work/github/db/pingcap/tidb \
  --rust-root . \
  --require pkg/parser,pkg/types,pkg/util/codec,pkg/tablecodec
```

## 报告字段

| 字段 | 含义 | 处理 |
|---|---|---|
| `missing-dir` | TiDB 目录在 Rust 缺失 | 建目录 + `mod.rs` + README/占位 |
| `not-started` | `.go` 无对应 `.rs` | 建占位或加入后续阶段 |
| `invalid` | `.rs` 无合法 `PORT_STATUS` | 补文件头 |
| `extra` | Rust 额外 `.rs` | 确认是否白名单（bridge/测试） |
| `stub` | 显式未实现 | 确认已登记进度 |
| `omitted` | 本阶段不移植 | 确认有原因/阶段 |
| `ported/partial/reimplemented` | 已移植状态 | 抽查文件头与测试 |

## 判定规则

1. **计算层目录缺失 = Gate 失败**（除非该阶段明确不要求）。
2. `invalid` 数量必须为 0。
3. `extra` 必须能解释（bridge、测试、工具）。
4. `stub` 必须同时出现在 `progress.md` 的“技术债/下一步”。
5. `omitted` 必须有 `PARITY_NOTES` 说明原因与计划阶段。

## 与进度联动

- 审计后更新 `project/parity-matrix.md` §2/§3 摘要。
- 若状态从 `stub → partial → ported`，同步更新 `progress.md`。
- 保存报告到 `project/reports/phase-XX/parity.json`（Gate 证据）。

## 禁止

- ❌ 手工修改报告数字而不改代码。
- ❌ 把 `not-started` 标成 `ported` 而无测试。
- ❌ 用 `omitted` 逃避本阶段必须实现的文件。

## 完成标准

- [ ] 报告生成成功。
- [ ] `invalid=0`；`missing-dir` 有解释。
- [ ] 状态变更已同步到进度/矩阵。
- [ ] 报告归档。
