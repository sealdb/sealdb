use clap::{Parser, Subcommand};
use sealdb_parser::parse_to_oracle_json;
use sealdb_session::Session;

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
    sql: Option<String>,
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
    if let Some(sql) = cli.execute.clone() {
        return execute_sql(&sql);
    }
    match cli.command {
        Some(Command::Parse { sql }) => print_parse(&sql, true),
        Some(Command::Execute { sql }) => execute_sql(&sql),
        None => {
            if cli.parse {
                let sql = cli
                    .sql
                    .ok_or_else(|| "missing --sql when using --parse".to_string())?;
                print_parse(&sql, cli.ast_json)
            } else if let Some(sql) = cli.sql {
                execute_sql(&sql)
            } else {
                println!("sealdb-server {}", env!("CARGO_PKG_VERSION"));
                Ok(())
            }
        }
    }
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

fn execute_sql(sql: &str) -> Result<(), String> {
    let session = Session::new();
    let result = session.execute_stmt(sql).map_err(|e| e.to_string())?;
    if result.columns.iter().any(|c| !c.is_empty()) {
        println!("{}", result.columns.join("\t"));
    }
    for row in result.display_rows() {
        println!("{}", row.join("\t"));
    }
    Ok(())
}
