mod config;

use clap::Parser;
use anyhow::Result;
use config::Config;

#[derive(Parser)]
#[command(name = "sealdb")]
#[command(about = "SealDB - 高性能分布式数据库")]
#[command(version)]
#[command(after_help = "示例用法:
  sealdb                           # 使用默认配置启动（无需配置文件）
  sealdb -c/--config config.yaml  # 使用指定配置文件
  sealdb -a/--host 127.0.0.1 -p/--port 8080  # 指定主机和端口
  sealdb -l/--log-level debug     # 设置日志级别
  sealdb -s/--show-config         # 显示当前配置
  sealdb -g/--generate-config     # 生成默认配置文件")]
struct Cli {
    /// 配置文件路径（可选，如果不存在则使用默认配置）
    #[arg(short, long, value_name = "FILE", default_value = "config.toml")]
    config: String,

    /// 服务器监听地址
    #[arg(short = 'a', long, value_name = "HOST")]
    host: Option<String>,

    /// 服务器监听端口
    #[arg(short = 'p', long, value_name = "PORT")]
    port: Option<u16>,

    /// 最大并发连接数
    #[arg(short = 'n', long, value_name = "CONNECTIONS")]
    max_connections: Option<u32>,

    /// 日志级别 (debug, info, warn, error)
    #[arg(short = 'l', long, value_name = "LEVEL", default_value = "info")]
    log_level: String,

    /// 是否输出日志到控制台
    #[arg(short = 'o', long)]
    console_log: bool,

    /// 日志文件路径
    #[arg(short = 'f', long, value_name = "FILE")]
    log_file: Option<String>,

    /// TiKV PD 端点列表
    #[arg(short = 'e', long, value_name = "ENDPOINTS", use_value_delimiter = true)]
    tikv_pd_endpoints: Option<Vec<String>>,

    /// TiKV 连接超时时间（毫秒）
    #[arg(short = 't', long, value_name = "TIMEOUT")]
    tikv_connect_timeout: Option<u64>,

    /// TiKV 请求超时时间（毫秒）
    #[arg(short = 'r', long, value_name = "TIMEOUT")]
    tikv_request_timeout: Option<u64>,

    /// 最大查询执行时间（毫秒）
    #[arg(short = 'q', long, value_name = "TIME")]
    max_query_time: Option<u64>,

    /// 最大内存使用量（字节）
    #[arg(short = 'm', long, value_name = "BYTES")]
    max_memory_usage: Option<u64>,

    /// 显示当前配置
    #[arg(short = 's', long)]
    show_config: bool,

    /// 生成默认配置文件
    #[arg(short = 'g', long)]
    generate_config: bool,

    /// 以守护进程模式运行
    #[arg(short = 'd', long)]
    daemon: bool,

    /// 指定 PID 文件路径
    #[arg(short = 'i', long, value_name = "FILE")]
    pid_file: Option<String>,

    /// 显示详细版本信息
    #[arg(long)]
    verbose_version: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // 处理版本信息
    if cli.verbose_version {
        println!("SealDB v{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // 处理生成配置文件
    if cli.generate_config {
        generate_default_configs()?;
        println!("默认配置文件已生成！");
        return Ok(());
    }

    // 加载配置
    let mut config = Config::from_file(&cli.config)?;

    // 应用命令行参数覆盖配置
    apply_cli_overrides(&mut config, &cli);

    // 验证配置
    validate_config(&config)?;

    if cli.show_config {
        println!("当前配置:");
        println!("{:#?}", config);
        return Ok(());
    }

    // 处理守护进程模式
    if cli.daemon {
        daemonize(&cli.pid_file)?;
    }

    // 启动服务
    start_server(&config, &cli)?;

    Ok(())
}

fn apply_cli_overrides(config: &mut Config, cli: &Cli) {
    // 服务器配置覆盖
    if let Some(host) = &cli.host {
        config.server.host = host.clone();
    }
    if let Some(port) = cli.port {
        config.server.port = port;
    }
    if let Some(max_connections) = cli.max_connections {
        config.server.max_connections = max_connections;
    }

    // 存储配置覆盖
    if let Some(endpoints) = &cli.tikv_pd_endpoints {
        config.storage.tikv_pd_endpoints = endpoints.clone();
    }
    if let Some(timeout) = cli.tikv_connect_timeout {
        config.storage.tikv_connect_timeout = timeout;
    }
    if let Some(timeout) = cli.tikv_request_timeout {
        config.storage.tikv_request_timeout = timeout;
    }

    // SQL 配置覆盖
    if let Some(query_time) = cli.max_query_time {
        config.sql.max_query_time = query_time;
    }
    if let Some(memory_usage) = cli.max_memory_usage {
        config.sql.max_memory_usage = memory_usage;
    }

    // 日志配置覆盖
    config.logging.level = cli.log_level.clone();
    if cli.console_log {
        config.logging.console = true;
    }
    if let Some(log_file) = &cli.log_file {
        config.logging.file = log_file.clone();
    }
}

fn validate_config(config: &Config) -> Result<()> {
    // 验证端口范围
    if config.server.port == 0 {
        return Err(anyhow::anyhow!("端口不能为 0"));
    }

    // 验证最大连接数
    if config.server.max_connections == 0 {
        return Err(anyhow::anyhow!("最大连接数不能为 0"));
    }

    // 验证 TiKV 端点
    if config.storage.tikv_pd_endpoints.is_empty() {
        return Err(anyhow::anyhow!("至少需要指定一个 TiKV PD 端点"));
    }

    // 验证日志级别
    let valid_levels = ["debug", "info", "warn", "error"];
    if !valid_levels.contains(&config.logging.level.as_str()) {
        return Err(anyhow::anyhow!("无效的日志级别: {}", config.logging.level));
    }

    Ok(())
}

fn daemonize(pid_file: &Option<String>) -> Result<()> {
    // 这里可以实现守护进程逻辑
    // 目前只是占位符
    if let Some(pid_path) = pid_file {
        println!("守护进程模式，PID 文件: {}", pid_path);
    } else {
        println!("守护进程模式");
    }
    Ok(())
}

fn start_server(config: &Config, cli: &Cli) -> Result<()> {
    println!("SealDB 启动成功！");
    println!("配置文件: {}", cli.config);
    println!("服务器地址: {}:{}", config.server.host, config.server.port);
    println!("最大连接数: {}", config.server.max_connections);
    println!("日志级别: {}", config.logging.level);
    println!("TiKV PD 端点: {:?}", config.storage.tikv_pd_endpoints);

    if cli.daemon {
        println!("运行模式: 守护进程");
    } else {
        println!("运行模式: 前台运行");
    }

    // TODO: 这里应该启动实际的服务器
    // 目前只是打印信息

    Ok(())
}

fn generate_default_configs() -> Result<()> {
    let config = Config::default();

    // 生成 TOML 配置文件
    config.save_to_file("config.toml")?;
    println!("已生成 config.toml");

    // 生成 JSON 配置文件
    config.save_to_file("config.json")?;
    println!("已生成 config.json");

    // 生成 YAML 配置文件
    config.save_to_file("config.yaml")?;
    println!("已生成 config.yaml");

    Ok(())
}
