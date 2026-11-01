use clap::{Parser, Subcommand};
use sealdb_config::Config;
use sealdb_domain::Domain;
use sealdb_parser::parse_to_oracle_json;
use sealdb_session::Session;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "sealdb-server", version, about = "SealDB compute-layer CLI")]
struct Cli {
    #[arg(long)]
    parse: bool,
    #[arg(long)]
    ast_json: bool,
    #[arg(long)]
    execute: Option<String>,
    #[arg(long)]
    listen: Option<String>,
    #[arg(long)]
    sql: Option<String>,
    /// Storage backend: `memory` (default, in-process) or `tikv`.
    #[arg(long)]
    store: Option<String>,
    /// PD endpoints for `--store tikv` (comma-separated). Default 127.0.0.1:2379.
    #[arg(long)]
    pd: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Parse { sql: String },
    Execute { sql: String },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let cfg = load_config(&cli)?;
    if let Some(sql) = cli.execute.clone() {
        return execute_sql(&cfg, &sql);
    }
    if let Some(addr) = cli.listen.clone() {
        return listen(&cfg, &addr);
    }
    match cli.command {
        Some(Command::Parse { sql }) => print_parse(&sql, true),
        Some(Command::Execute { sql }) => execute_sql(&cfg, &sql),
        None => {
            if cli.parse {
                let sql = cli
                    .sql
                    .ok_or_else(|| "missing --sql when using --parse".to_string())?;
                print_parse(&sql, cli.ast_json)
            } else if let Some(sql) = cli.sql {
                execute_sql(&cfg, &sql)
            } else {
                println!("sealdb-server {}", env!("CARGO_PKG_VERSION"));
                Ok(())
            }
        }
    }
}

fn load_config(cli: &Cli) -> Result<Config, String> {
    Config::from_env()
        .and_then(|cfg| cfg.with_store_name(cli.store.as_deref()))
        .map(|cfg| cfg.with_pd(cli.pd.as_deref()))
        .map_err(|e| e.to_string())
}

fn open_domain(cfg: &Config) -> Result<Arc<Domain>, String> {
    let storage = sealdb_store::open(cfg).map_err(|e| e.to_string())?;
    Ok(Arc::new(Domain::with_storage(storage)))
}

fn print_parse(sql: &str, pretty: bool) -> Result<(), String> {
    let out = parse_to_oracle_json(sql);
    let text = if pretty {
        serde_json::to_string_pretty(&out).map_err(|e| e.to_string())?
    } else {
        serde_json::to_string(&out).map_err(|e| e.to_string())?
    };
    println!("{text}");
    Ok(())
}

fn listen(cfg: &Config, addr: &str) -> Result<(), String> {
    let domain = open_domain(cfg)?;
    let server =
        sealdb_server::Server::bind_with_domain(addr, domain).map_err(|e| e.to_string())?;
    eprintln!(
        "SealDB MySQL protocol listening on {} store={} pd={}",
        server.local_addr().map_err(|e| e.to_string())?,
        cfg.store.as_str(),
        cfg.pd_endpoints.join(","),
    );
    server.serve().map_err(|e| e.to_string())
}

fn execute_sql(cfg: &Config, sql: &str) -> Result<(), String> {
    let session = Session::with_domain(open_domain(cfg)?);
    let result = session.execute_stmt(sql).map_err(|e| e.to_string())?;
    if result.columns.iter().any(|c| !c.is_empty()) {
        println!("{}", result.columns.join("\t"));
    }
    for row in result.display_rows() {
        println!("{}", row.join("\t"));
    }
    Ok(())
}
