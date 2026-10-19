# 项目 Skills

本目录存放**与 TiDB 移植强相关**的可复用工作流。每个 `.md` 文件带 YAML frontmatter，可被 opencode 作为 Skill 加载。

## 安装到 opencode

```bash
./project/scripts/install-skills.sh
# 生成：
# .opencode/skills/port-tidb-file/SKILL.md
# .opencode/skills/parity-audit/SKILL.md
# ...
```

脚本默认使用**符号链接**；如环境不支持，可改为复制：

```bash
./project/scripts/install-skills.sh --copy
```

## 清单

| Skill | 用途 | 触发场景 |
|---|---|---|
| `port-tidb-file` | 把一个 Go 文件移植为 Rust | “移植 `pkg/.../foo.go`” |
| `parity-audit` | 目录/文件/状态审计 | “检查对齐/缺哪些文件” |
| `oracle-diff` | 运行 TiDB Oracle 并差分 | “结果/计划不一致” |
| `plan-debug` | 优化器/执行器分层排障 | “EXPLAIN 不对/计划选错” |
| `phase-gate` | 阶段验收与报告 | “Phase X 是否可以收尾” |

## 编写约束

- 每个 Skill 只解决一个明确任务，步骤可执行、可验证。
- 必须引用项目文档（`requirements.md`/`conventions.md`/`testing.md`），不要复制大段规范。
- 必须包含“禁止事项”和“完成标准”。
- 涉及命令时给可复制示例；路径用仓库相对路径。
