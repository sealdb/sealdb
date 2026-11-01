use clap::Parser;
use sealdb_integrationtest::{run_all, testdata_root};

#[derive(Parser)]
#[command(
    name = "sealdb-itest",
    about = "TiDB-style SQL integration tester (t/*.test vs r/*.result)"
)]
struct Cli {
    /// Rewrite r/*.result from the current Session output.
    #[arg(long, short = 'r')]
    record: bool,
    /// Run only suites whose name contains this substring.
    #[arg(long, short = 't')]
    case: Option<String>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let n =
        run_all(&testdata_root(), cli.record, cli.case.as_deref()).map_err(|e| e.to_string())?;
    if cli.record {
        eprintln!("recorded {n} suite(s)");
    } else {
        eprintln!("PASS: {n} suite(s)");
    }
    Ok(())
}
