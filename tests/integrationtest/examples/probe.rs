//! Scratch SQL probe: `cargo run -p sealdb-integrationtest --example probe -- '<sql>' ...`

use std::sync::Arc;

use sealdb_config::Config;
use sealdb_domain::Domain;
use sealdb_session::Session;

fn main() {
    let cfg = Config::default().with_store_name(Some("memory")).unwrap();
    let storage = sealdb_store::open(&cfg).unwrap();
    let domain = Arc::new(
        Domain::with_storage_id_base(storage, 12345).expect("domain meta load"),
    );
    let session = Session::with_domain(domain);
    let _ = session.execute_stmt("DROP DATABASE IF EXISTS `probe`");
    let _ = session.execute_stmt("CREATE DATABASE IF NOT EXISTS `probe`");
    let _ = session.execute_stmt("USE `probe`");
    for sql in std::env::args().skip(1) {
        match session.execute_stmt(&sql) {
            Ok(qr) => {
                println!("{}", sql);
                if !qr.columns.is_empty() {
                    println!("{}", qr.columns.join("\t"));
                }
                for row in qr.display_rows() {
                    println!("{}", row.join("\t"));
                }
            }
            Err(err) => println!("{} -> ERROR {}: {}", sql, err.code, err.message),
        }
    }
}
