# ADR-0006: Rust 1.99.1 / Edition 2024

- 状态：**accepted**
- 日期：2026-10-19
- 关联：requirements Q3；risks R8

## 背景

初始环境为 Rust/Cargo 1.80.1，而仓库使用了 edition 2024，导致 `cargo metadata` 无法解析。  
用户要求优先使用较新的 Rust，除非环境不匹配或无法升级。

升级尝试：

- 默认源 `static.rust-lang.org` 超时；
- 使用 `rsproxy.cn` 镜像成功安装 Rust **1.99.1**（2026-10-13）。

## 决策

采用：

- **Rust 1.99.1**
- **Edition 2024**
- **Resolver 3**
- `rust-toolchain.toml` 固定 `channel = "1.99.1"`，组件 `rustfmt`、`clippy`
- workspace `rust-version = "1.99"`

## 后果

### 正面

- 使用近期语言特性与编译器改进。
- 本机与 CI 可通过 `rust-toolchain.toml` 复现。
- 不再受 edition 2024 解析问题阻塞。

### 负面 / 代价

- 团队成员/CI 需要能安装 1.99.1；网络受限时需配置镜像。
- 部分旧 crate 可能与 edition 2024 不兼容（引入依赖时验证）。

### 环境说明

```bash
RUSTUP_DIST_SERVER=https://rsproxy.cn \
RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup \
rustup toolchain install 1.99.1 --profile minimal --component rustfmt --component clippy
```

## 备选方案

| 方案 | 结论 |
|---|---|
| edition 2021 + 1.80.1 | 可用但落后；用户要求较新版本，未采用 |
| nightly | 不稳定，未采用 |
| 不固定工具链 | 不可复现，未采用 |

## 验证方式

```bash
rustc --version    # 1.99.1
cargo --version    # 1.99.1
cargo metadata --no-deps --format-version 1
cargo check --workspace
```

Phase 0 已通过。

## 参考

- `rust-toolchain.toml`
- `Cargo.toml`
