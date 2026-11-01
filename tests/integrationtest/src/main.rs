use clap::Parser;
use sealdb_integrationtest::{RunOptions, StoreBackend, run_all, testdata_root};

#[derive(Parser)]
#[command(
    name = "sealdb-itest",
    about = "TiDB-style SQL integration tester (t/*.test vs r/*.result)"
)]
struct Cli {
    /// Rewrite r/*.result from MemStorage output.
    #[arg(long, short = 'r')]
    record: bool,
    /// Run only suites whose name contains this substring.
    #[arg(long, short = 't')]
    case: Option<String>,
    /// Store backends: memory, tikv, or both (default).
    #[arg(long, short = 's', default_value = "both")]
    store: String,
    /// Fail if TiKV/PD is unreachable instead of skipping.
    #[arg(long)]
    require_tikv: bool,
    /// PD endpoints for `--store tikv` (default 127.0.0.1:2379).
    #[arg(long)]
    pd: Option<String>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let stores = StoreBackend::parse_list(&cli.store).map_err(|e| e.to_string())?;
    let report = run_all(
        &testdata_root(),
        &RunOptions {
            record: cli.record,
            filter: cli.case,
            stores,
            require_tikv: cli.require_tikv,
            pd: cli.pd,
        },
    )
    .map_err(|e| e.to_string())?;
    if report.tikv_skipped && report.ran() == 0 {
        return Err("tikv unavailable".to_string());
    }
    if cli.record {
        eprintln!(
            "recorded {} memory suite(s); tikv verified {}",
            report.memory,
            if report.tikv_skipped {
                "skipped".to_string()
            } else {
                format!("{}", report.tikv)
            }
        );
    } else {
        eprintln!(
            "PASS: memory={} tikv={}{}",
            report.memory,
            report.tikv,
            if report.tikv_skipped {
                " (tikv skipped)"
            } else {
                ""
            }
        );
    }
    Ok(())
}
