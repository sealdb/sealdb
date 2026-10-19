# 调试与设计笔记

本目录存放**非正式但可追溯**的排障/设计笔记，避免只留在聊天记录里。

命名：

```text
notes/
├── debug-YYYYMMDD-<slug>.md
├── design-YYYYMMDD-<slug>.md
└── upstream-YYYYMMDD-<slug>.md
```

模板：

```markdown
# 标题

- 日期：
- 阶段：
- 关联 Issue/ADR：

## 问题 / 目标
## 观察与证据
## 根因 / 结论
## 行动
## 遗留
```

注意：

- 笔记不是规范；结论稳定后应沉淀到 ADR/`architecture.md`/`testing.md`。
- 不要记录密钥、密码、用户数据。
