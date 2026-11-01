use sealdb_errno::Result;
use sealdb_types::Datum;
use std::collections::HashSet;

pub trait AggFunc {
    fn update(&mut self, value: Option<&Datum>) -> Result<()>;
    fn finalize(&self) -> Result<Datum>;
}

pub(crate) fn remember_distinct(seen: &mut HashSet<String>, value: &Datum) -> bool {
    seen.insert(format!("{value:?}"))
}

pub fn is_agg_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
    )
}
