# 测试环境 Dockerfile
# 使用官方 Rust 镜像作为构建阶段
FROM rust:1.75-slim as builder

# 安装系统依赖
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# 设置工作目录
WORKDIR /app

# 复制 Cargo.toml 和 Cargo.lock
COPY Cargo.toml Cargo.lock ./

# 复制所有源代码
COPY . .

# 构建项目并运行测试
RUN cargo test --no-run
RUN cargo build --release

# 使用轻量级的运行时镜像
FROM debian:bookworm-slim

# 安装运行时依赖
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# 创建非 root 用户
RUN useradd -r -s /bin/false sealdb

# 设置工作目录
WORKDIR /app

# 从构建阶段复制二进制文件
COPY --from=builder /app/target/release/sealdb /app/sealdb

# 创建必要的目录
RUN mkdir -p /app/logs /app/data && \
    chown -R sealdb:sealdb /app

# 切换到非 root 用户
USER sealdb

# 暴露端口
EXPOSE 4000

# 设置环境变量
ENV RUST_LOG=debug
ENV SEALDB_CONFIG=/app/config.toml

# 启动命令
CMD ["/app/sealdb"] 