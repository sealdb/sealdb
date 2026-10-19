# Gate / 审计报告

本目录存放阶段验收与审计证据。命名：

```text
reports/
├── phase-00/
│   ├── summary.md
│   ├── parity.json
│   ├── oracle.json
│   └── *.log
├── phase-01/
└── ...
```

规则：

- 每个阶段 Gate 必须生成 `summary.md` 与机器可读 JSON。
- 报告中的环境版本（Rust/PD/TiKV/TiDB）必须写清楚。
- 失败报告也要保留，便于追溯。
- 不要把密钥、连接串、用户数据写入报告。
- 报告由 `project/scripts/phase_gate.sh` / `oracle_diff.sh` 生成，避免手工伪造。
