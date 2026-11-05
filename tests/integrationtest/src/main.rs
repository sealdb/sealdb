use clap::Parser;
use sealdb_integrationtest::{RunOptions, StoreBackend, run_all, testdata_root};

#[derive(Parser)]
#[command(
    name = "sealdb-itest",
    about = "TiDB-style SQL integration tester (t/**/*.test vs r/**/*.result)"
)]
struct Cli {
    /// Rewrite r/*.result from SealDB MemStorage (FORBIDDEN for TiDB corpus).
    /// Requires SEALDB_ITEST_ALLOW_SEALDB_RECORD=1 and only `local/` suites.
    /// Authoritative goldens: project/scripts/record_tidb_itest.sh
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
    /// Manifest statuses to run (comma-separated). Default: green,local
    /// (+ in_progress/pending via env SEALDB_ITEST_RUN_*).
    #[arg(long)]
    status: Option<String>,
    /// Manifest waves to run (comma-separated), e.g. R1,R2.
    #[arg(long)]
    wave: Option<String>,
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
    let statuses = cli.status.as_ref().map(|s| {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    });
    let waves = cli.wave.as_ref().map(|s| {
        s.split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect()
    });
    let report = run_all(
        &testdata_root(),
        &RunOptions {
            record: cli.record,
            filter: cli.case,
            stores,
            require_tikv: cli.require_tikv,
            pd: cli.pd,
            statuses,
            waves,
            require_golden: true,
        },
    )
    .map_err(|e| e.to_string())?;
    if report.tikv_skipped && report.ran() == 0 {
        return Err("tikv unavailable".to_string());
    }
    if cli.record {
        eprintln!(
            "recorded {} memory suite(s); tikv verified {}; skipped={}",
            report.memory,
            if report.tikv_skipped {
                "skipped".to_string()
            } else {
                format!("{}", report.tikv)
            },
            report.skipped_suites
        );
    } else {
        eprintln!(
            "PASS: memory={} tikv={} skipped={}{}",
            report.memory,
            report.tikv,
            report.skipped_suites,
            if report.tikv_skipped {
                " (tikv skipped)"
            } else {
                ""
            }
        );
    }
    Ok(())
}
