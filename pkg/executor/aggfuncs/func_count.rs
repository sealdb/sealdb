use super::aggfuncs::{AggFunc, remember_distinct};
use sealdb_errno::Result;
use sealdb_types::Datum;
use std::collections::HashSet;

#[derive(Default)]
pub struct Count {
    count: i64,
    star: bool,
    distinct: bool,
    seen: HashSet<String>,
}

impl Count {
    pub fn new(star: bool) -> Self {
        Self {
            count: 0,
            star,
            distinct: false,
            seen: HashSet::new(),
        }
    }

    pub fn distinct() -> Self {
        Self {
            count: 0,
            star: false,
            distinct: true,
            seen: HashSet::new(),
        }
    }
}

impl AggFunc for Count {
    fn update(&mut self, value: Option<&Datum>) -> Result<()> {
        if self.star {
            self.count += 1;
            return Ok(());
        }
        let Some(v) = value else {
            return Ok(());
        };
        if v.is_null() {
            return Ok(());
        }
        if self.distinct {
            if remember_distinct(&mut self.seen, v) {
                self.count += 1;
            }
            return Ok(());
        }
        self.count += 1;
        Ok(())
    }

    fn finalize(&self) -> Result<Datum> {
        Ok(Datum::I64(self.count))
    }
}
