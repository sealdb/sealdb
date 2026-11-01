use super::aggfuncs::{AggFunc, remember_distinct};
use sealdb_errno::Result;
use sealdb_types::Datum;
use std::collections::HashSet;

#[derive(Default)]
pub struct Sum {
    sum: f64,
    seen: bool,
    distinct: bool,
    distinct_seen: HashSet<String>,
}

impl Sum {
    pub fn distinct() -> Self {
        Self {
            distinct: true,
            ..Self::default()
        }
    }
}

impl AggFunc for Sum {
    fn update(&mut self, value: Option<&Datum>) -> Result<()> {
        let Some(v) = value else {
            return Ok(());
        };
        if v.is_null() {
            return Ok(());
        }
        if self.distinct && !remember_distinct(&mut self.distinct_seen, v) {
            return Ok(());
        }
        self.sum += v.as_f64()?;
        self.seen = true;
        Ok(())
    }

    fn finalize(&self) -> Result<Datum> {
        if self.seen {
            if self.sum.fract() == 0.0 && self.sum.is_finite() {
                Ok(Datum::I64(self.sum as i64))
            } else {
                Ok(Datum::F64(self.sum))
            }
        } else {
            Ok(Datum::Null)
        }
    }
}
