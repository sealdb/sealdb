use sealdb_errno::Result;
use sealdb_types::Datum;

pub trait AggFunc {
    fn update(&mut self, value: Option<&Datum>) -> Result<()>;
    fn finalize(&self) -> Result<Datum>;
}

pub fn is_agg_name(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
    )
}
