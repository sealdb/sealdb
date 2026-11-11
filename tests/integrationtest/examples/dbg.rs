//! Scratch runner: execute SQL statements against an in-memory store.
//!
//! `cargo run -q -p sealdb-integrationtest --example dbg -- "create table …" "select …"`

use sealdb_config::Config;
use sealdb_domain::Domain;
use sealdb_session::Session;
use std::sync::Arc;

fn main() {
    let cfg = Config::default()
        .with_store_name(Some("memory"))
        .expect("store config");
    let storage = sealdb_store::open(&cfg).expect("open store");
    let domain =
        Arc::new(Domain::with_storage_and_grant_table_id_base(storage, 0).expect("domain"));
    let session = Session::with_domain(domain);
    for sql in std::env::args().skip(1) {
        println!("-- {sql}");
        match session.execute_stmt(&sql) {
            Ok(qr) => {
                if !qr.columns.is_empty() {
                    println!("{}", qr.columns.join("\t"));
                }
                for row in qr.display_rows() {
                    println!("{}", row.join("\t"));
                }
            }
            Err(e) => println!("Error {} ({}): {}", e.code, e.sqlstate, e.message),
        }
    }
}
