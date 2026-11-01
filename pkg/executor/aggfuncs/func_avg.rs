use super::aggfuncs::{AggFunc, remember_distinct};
use sealdb_errno::Result;
use sealdb_types::Datum;
use std::collections::HashSet;

#[derive(Default)]
pub struct Avg {
    sum: f64,
    count: i64,
    distinct: bool,
    seen: HashSet<String>,
}

impl Avg {
    pub fn distinct() -> Self {
        Self {
            distinct: true,
            ..Self::default()
        }
    }
}

impl AggFunc for Avg {
    fn update(&mut self, value: Option<&Datum>) -> Result<()> {
        let Some(v) = value else {
            return Ok(());
        };
        if v.is_null() {
            return Ok(());
        }
        if self.distinct && !remember_distinct(&mut self.seen, v) {
            return Ok(());
        }
        self.sum += v.as_f64()?;
        self.count += 1;
        Ok(())
    }

    fn finalize(&self) -> Result<Datum> {
        if self.count == 0 {
            Ok(Datum::Null)
        } else {
            Ok(Datum::F64(self.sum / self.count as f64))
        }
    }
}
